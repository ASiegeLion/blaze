// Copyright 2022 The Blaze Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::shuffle::{
    evaluate_hashes, evaluate_partition_ids, ShuffleRepartitioner, ShuffleSpill,
};
use arrow::compute;
use arrow::datatypes::SchemaRef;
use arrow::error::Result as ArrowResult;
use arrow::record_batch::RecordBatch;
use async_trait::async_trait;
use datafusion::common::Result;
use datafusion::execution::context::TaskContext;
use datafusion::physical_plan::metrics::BaselineMetrics;
use datafusion::physical_plan::Partitioning;
use datafusion_ext_commons::io::write_one_batch;
use datafusion_ext_commons::loser_tree::LoserTree;
use derivative::Derivative;
use futures::lock::Mutex;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Cursor, Read, Seek, Write};
use std::sync::{Arc, Weak};
use crate::common::memory_manager::{MemConsumer, MemConsumerInfo, MemManager};
use crate::common::onheap_spill::OnHeapSpill;

// reserve memory for each spill
// estimated size: bufread=64KB + sizeof(offsets)=~KBs
const SPILL_OFFHEAP_MEM_COST: usize = 70000;

pub struct SortShuffleRepartitioner {
    mem_consumer_info: Option<Weak<MemConsumerInfo>>,
    output_data_file: String,
    output_index_file: String,
    schema: SchemaRef,
    buffered_batches: Mutex<Vec<RecordBatch>>,
    spills: Mutex<Vec<ShuffleSpill>>,
    partitioning: Partitioning,
    num_output_partitions: usize,
    batch_size: usize,
    metrics: BaselineMetrics,
}

impl SortShuffleRepartitioner {
    pub fn new(
        output_data_file: String,
        output_index_file: String,
        schema: SchemaRef,
        partitioning: Partitioning,
        metrics: BaselineMetrics,
        context: Arc<TaskContext>,
    ) -> Self {
        let num_output_partitions = partitioning.partition_count();
        let batch_size = context.session_config().batch_size();
        let repartitioner = Self {
            mem_consumer_info: None,
            output_data_file,
            output_index_file,
            schema,
            buffered_batches: Mutex::default(),
            spills: Mutex::default(),
            partitioning,
            num_output_partitions,
            batch_size,
            metrics,
        };
        repartitioner
    }

    fn spill_buffered_batches(
        &self,
        buffered_batches: &[RecordBatch],
    ) -> Result<Option<ShuffleSpill>> {

        if buffered_batches.is_empty() {
            return Ok(None);
        }

        // combine all buffered batches
        let num_output_partitions = self.num_output_partitions;
        let num_buffered_rows = buffered_batches
            .iter()
            .map(|batch| batch.num_rows())
            .sum::<usize>();

        let mut pi_vec = Vec::with_capacity(num_buffered_rows);
        for (batch_idx, batch) in buffered_batches.iter().enumerate() {
            let hashes = evaluate_hashes(&self.partitioning, &batch)?;
            let partition_ids = evaluate_partition_ids(&hashes, num_output_partitions);

            // compute partition ids and sorted indices
            pi_vec.extend(hashes
                .into_iter()
                .zip(partition_ids.into_iter())
                .enumerate()
                .map(|(i, (hash, partition_id))| PI {
                    partition_id,
                    hash,
                    batch_idx: batch_idx as u32,
                    row_idx: i as u32,
                }));
        }
        pi_vec.sort_unstable();

        // write to in-mem spill
        let mut buffered_columns = vec![vec![]; buffered_batches[0].num_columns()];
        buffered_batches.iter().for_each(|batch| batch
            .columns()
            .iter()
            .enumerate()
            .for_each(|(col_idx, col)| buffered_columns[col_idx].push(col.as_ref())));

        let mut cur_partition_id = 0;
        let mut cur_slice_start = 0;
        let cur_spill = OnHeapSpill::try_new()?;
        let mut cur_spill_writer = cur_spill.get_buf_writer();
        let mut cur_spill_offsets = vec![0];
        let mut offset = 0;

        macro_rules! write_sub_batch {
            ($range:expr) => {{
                let sub_pi_vec = &pi_vec[$range];
                let sub_indices = sub_pi_vec
                    .iter()
                    .map(|pi| (pi.batch_idx as usize, pi.row_idx as usize))
                    .collect::<Vec<_>>();

                let sub_batch = RecordBatch::try_new(
                    self.schema.clone(),
                    buffered_columns
                        .iter()
                        .map(|columns| compute::interleave(columns, &sub_indices))
                        .collect::<ArrowResult<Vec<_>>>()?,
                )?;
                let mut buf = vec![];
                write_one_batch(&sub_batch, &mut Cursor::new(&mut buf), true)?;
                offset += buf.len() as u64;
                cur_spill_writer.write(&buf)?;
            }};
        }

        // write sorted data into in-mem spill
        for cur_offset in 0..pi_vec.len() {
            if pi_vec[cur_offset].partition_id > cur_partition_id
                || cur_offset - cur_slice_start >= self.batch_size
            {
                if cur_slice_start < cur_offset {
                    write_sub_batch!(cur_slice_start..cur_offset);
                    cur_slice_start = cur_offset;
                }
                while pi_vec[cur_offset].partition_id > cur_partition_id {
                    cur_spill_offsets.push(offset);
                    cur_partition_id += 1;
                }
            }
        }
        if cur_slice_start < pi_vec.len() {
            write_sub_batch!(cur_slice_start..);
        }

        // add one extra offset at last to ease partition length computation
        cur_spill_offsets.resize(num_output_partitions + 1, offset);

        drop(cur_spill_writer);
        cur_spill.complete()?;

        Ok(Some(ShuffleSpill {
            spill: cur_spill,
            offsets: cur_spill_offsets,
        }))
    }
}

#[async_trait]
impl MemConsumer for SortShuffleRepartitioner {
    fn set_consumer_info(&mut self, consumer_info: Weak<MemConsumerInfo>) {
        self.mem_consumer_info = Some(consumer_info);
    }

    fn get_consumer_info(&self) -> &Weak<MemConsumerInfo> {
        &self.mem_consumer_info.as_ref().expect("consumer info net set")
    }

    async fn spill(&self) -> Result<()> {
        let mut batches = self.buffered_batches.lock().await;

        self.spills.lock().await.extend(
            self.spill_buffered_batches(&std::mem::take(&mut *batches))?
        );
        self.update_mem_used(0).await?;
        Ok(())
    }
}

impl Drop for SortShuffleRepartitioner {
    fn drop(&mut self) {
        MemManager::deregister_consumer(self);
    }
}

#[async_trait]
impl ShuffleRepartitioner for SortShuffleRepartitioner {
    async fn insert_batch(&self, input: RecordBatch) -> Result<()> {
        let mem_increase =
            input.get_array_memory_size() +
            input.num_rows() * std::mem::size_of::<PI>(); // for sorting
        self.update_mem_used_with_diff(mem_increase as isize).await?;
        self.buffered_batches.lock().await.push(input);
        Ok(())
    }

    async fn shuffle_write(&self) -> Result<()> {
        self.set_spillable(false);
        let mut spills = std::mem::take(&mut *self.spills.lock().await);
        let buffered_batches =
            std::mem::take(&mut *self.buffered_batches.lock().await);

        // spill all buffered batches
        if let Some(spill) = self.spill_buffered_batches(&buffered_batches)? {
            spills.push(spill);
        }
        log::info!("sort repartitioner starts outputting with {} spills", spills.len());

        // adjust mem usage
        self.update_mem_used(spills.len() * SPILL_OFFHEAP_MEM_COST).await?;

        // define spill cursor. partial-ord is reversed because we
        // need to find mininum using a binary heap
        #[derive(Derivative)]
        #[derivative(PartialOrd, PartialEq, Ord, Eq)]
        struct SpillCursor {
            cur: usize,

            #[derivative(PartialOrd = "ignore")]
            #[derivative(PartialEq = "ignore")]
            #[derivative(Ord = "ignore")]
            reader: BufReader<Box<dyn Read + Send>>,

            #[derivative(PartialOrd = "ignore")]
            #[derivative(PartialEq = "ignore")]
            #[derivative(Ord = "ignore")]
            offsets: Vec<u64>,
        }
        impl SpillCursor {
            fn skip_empty_partitions(&mut self) {
                let offsets = &self.offsets;
                while !self.finished() && offsets[self.cur + 1] == offsets[self.cur] {
                    self.cur += 1;
                }
            }

            fn finished(&self) -> bool {
                self.cur + 1 >= self.offsets.len()
            }
        }

        let raw_spills: Vec<OnHeapSpill> = spills
            .iter()
            .map(|spill| spill.spill.clone())
            .collect();

        // use loser tree to select partitions from spills
        let mut cursors: LoserTree<SpillCursor> = LoserTree::new_by(
            spills
                .into_iter()
                .map(|spill| {
                    let mut cursor = SpillCursor {
                        cur: 0,
                        reader: spill.spill.get_buf_reader(),
                        offsets: spill.offsets,
                    };
                    cursor.skip_empty_partitions();
                    cursor
                })
                .filter(|spill| !spill.finished())
                .collect(),
            |c1: &SpillCursor, c2: &SpillCursor| match (c1, c2) {
                (c1, _2) if c1.finished() => false,
                (_1, c2) if c2.finished() => true,
                (c1, c2) => c1 < c2,
            },
        );

        let data_file = self.output_data_file.clone();
        let index_file = self.output_index_file.clone();

        let num_output_partitions = self.num_output_partitions;
        let mut offsets = vec![0];
        let mut output_data = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(data_file)?;
        let mut cur_partition_id = 0;

        // append partition in each spills
        if cursors.len() > 0 {
            loop {
                let mut min_spill = cursors.peek_mut();
                if min_spill.finished() {
                    break;
                }

                while cur_partition_id < min_spill.cur {
                    offsets.push(output_data.stream_position()?);
                    cur_partition_id += 1;
                }
                let (spill_offset_start, spill_offset_end) = (
                    min_spill.offsets[cur_partition_id],
                    min_spill.offsets[cur_partition_id + 1],
                );

                let spill_range = spill_offset_start as usize..spill_offset_end as usize;
                let reader = &mut min_spill.reader;
                std::io::copy(
                    &mut reader.take(spill_range.len() as u64),
                    &mut output_data,
                )?;

                // forward partition id in min_spill
                min_spill.cur += 1;
                min_spill.skip_empty_partitions();
            }
        }
        output_data.flush()?;

        // add one extra offset at last to ease partition length computation
        offsets.resize(num_output_partitions + 1, output_data.stream_position()?);

        let mut output_index = File::create(index_file)?;
        for offset in offsets {
            output_index.write_all(&(offset as i64).to_le_bytes()[..])?;
        }
        output_index.flush()?;

        // update disk spill size
        let spill_disk_usage = raw_spills
            .iter()
            .map(|spill| spill.get_disk_usage().unwrap_or(0))
            .sum::<u64>();
        self.metrics.record_spill(spill_disk_usage as usize);
        self.update_mem_used(0).await?;
        Ok(())
    }
}

#[derive(Derivative)]
#[derivative(Clone, Copy, Default, PartialOrd, PartialEq, Ord, Eq)]
struct PI {
    partition_id: u32,
    hash: u32,

    #[derivative(PartialOrd = "ignore")]
    #[derivative(PartialEq = "ignore")]
    #[derivative(Ord = "ignore")]
    batch_idx: u32,

    #[derivative(PartialOrd = "ignore")]
    #[derivative(PartialEq = "ignore")]
    #[derivative(Ord = "ignore")]
    row_idx: u32,
}