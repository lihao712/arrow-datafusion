// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! Aggregate without grouping columns

use crate::execution::context::TaskContext;
use crate::physical_plan::aggregates::{
    aggregate_expressions, create_accumulators, finalize_aggregation, AccumulatorItem,
    AggregateMode,
};
use crate::physical_plan::metrics::{BaselineMetrics, RecordOutput};
use crate::physical_plan::{RecordBatchStream, SendableRecordBatchStream};
use arrow::datatypes::SchemaRef;
use arrow::record_batch::RecordBatch;
use datafusion_common::Result;
use datafusion_physical_expr::{AggregateExpr, PhysicalExpr};
use futures::stream::BoxStream;
use std::borrow::Cow;
use std::sync::Arc;
use std::task::{Context, Poll};

use crate::execution::memory_pool::{MemoryConsumer, MemoryReservation};
use crate::physical_plan::filter::batch_filter;
use futures::stream::{Stream, StreamExt};

/// stream struct for aggregation without grouping columns
pub(crate) struct AggregateStream {
    stream: BoxStream<'static, Result<RecordBatch>>,
    schema: SchemaRef,
}

/// Actual implementation of [`AggregateStream`].
///
/// This is wrapped into yet another struct because we need to interact with the async memory management subsystem
/// during poll. To have as little code "weirdness" as possible, we chose to just use [`BoxStream`] together with
/// [`futures::stream::unfold`].
///
/// The latter requires a state object, which is [`AggregateStreamInner`].
struct AggregateStreamInner {
    schema: SchemaRef,
    mode: AggregateMode,
    input: SendableRecordBatchStream,
    baseline_metrics: BaselineMetrics,
    aggregate_expressions: Vec<Vec<Arc<dyn PhysicalExpr>>>,
    filter_expressions: Vec<Option<Arc<dyn PhysicalExpr>>>,
    accumulators: Vec<AccumulatorItem>,
    reservation: MemoryReservation,
    finished: bool,
}

impl AggregateStream {
    #[allow(clippy::too_many_arguments)]
    /// Create a new AggregateStream
    pub fn new(
        mode: AggregateMode,
        schema: SchemaRef,
        aggr_expr: Vec<Arc<dyn AggregateExpr>>,
        filter_expr: Vec<Option<Arc<dyn PhysicalExpr>>>,
        input: SendableRecordBatchStream,
        baseline_metrics: BaselineMetrics,
        context: Arc<TaskContext>,
        partition: usize,
    ) -> Result<Self> {
        let aggregate_expressions = aggregate_expressions(&aggr_expr, &mode, 0)?;
        let filter_expressions = match mode {
            AggregateMode::Partial | AggregateMode::Single => filter_expr,
            AggregateMode::Final | AggregateMode::FinalPartitioned => {
                vec![None; aggr_expr.len()]
            }
        };
        let accumulators = create_accumulators(&aggr_expr)?;

        let reservation = MemoryConsumer::new(format!("AggregateStream[{partition}]"))
            .register(context.memory_pool());

        let inner = AggregateStreamInner {
            schema: Arc::clone(&schema),
            mode,
            input,
            baseline_metrics,
            aggregate_expressions,
            filter_expressions,
            accumulators,
            reservation,
            finished: false,
        };
        let stream = futures::stream::unfold(inner, |mut this| async move {
            if this.finished {
                return None;
            }

            let elapsed_compute = this.baseline_metrics.elapsed_compute();

            loop {
                let result = match this.input.next().await {
                    Some(Ok(batch)) => {
                        let timer = elapsed_compute.timer();
                        let result = aggregate_batch(
                            &this.mode,
                            batch,
                            &mut this.accumulators,
                            &this.aggregate_expressions,
                            &this.filter_expressions,
                        );

                        timer.done();

                        // allocate memory
                        // This happens AFTER we actually used the memory, but simplifies the whole accounting and we are OK with
                        // overshooting a bit. Also this means we either store the whole record batch or not.
                        match result
                            .and_then(|allocated| this.reservation.try_grow(allocated))
                        {
                            Ok(_) => continue,
                            Err(e) => Err(e),
                        }
                    }
                    Some(Err(e)) => Err(e),
                    None => {
                        this.finished = true;
                        let timer = this.baseline_metrics.elapsed_compute().timer();
                        let result = finalize_aggregation(&this.accumulators, &this.mode)
                            .and_then(|columns| {
                                RecordBatch::try_new(this.schema.clone(), columns)
                                    .map_err(Into::into)
                            })
                            .record_output(&this.baseline_metrics);

                        timer.done();

                        result
                    }
                };

                this.finished = true;
                return Some((result, this));
            }
        });

        // seems like some consumers call this stream even after it returned `None`, so let's fuse the stream.
        let stream = stream.fuse();
        let stream = Box::pin(stream);

        Ok(Self { schema, stream })
    }
}

impl Stream for AggregateStream {
    type Item = Result<RecordBatch>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        this.stream.poll_next_unpin(cx)
    }
}

impl RecordBatchStream for AggregateStream {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }
}

/// Perform group-by aggregation for the given [`RecordBatch`].
///
/// If successful, this returns the additional number of bytes that were allocated during this process.
///
/// TODO: Make this a member function
fn aggregate_batch(
    mode: &AggregateMode,
    batch: RecordBatch,
    accumulators: &mut [AccumulatorItem],
    expressions: &[Vec<Arc<dyn PhysicalExpr>>],
    filters: &[Option<Arc<dyn PhysicalExpr>>],
) -> Result<usize> {
    let mut allocated = 0usize;

    // 1.1 iterate accumulators and respective expressions together
    // 1.2 filter the batch if necessary
    // 1.3 evaluate expressions
    // 1.4 update / merge accumulators with the expressions' values

    // 1.1
    accumulators
        .iter_mut()
        .zip(expressions)
        .zip(filters)
        .try_for_each(|((accum, expr), filter)| {
            // 1.2
            let batch = match filter {
                Some(filter) => Cow::Owned(batch_filter(&batch, filter)?),
                None => Cow::Borrowed(&batch),
            };
            // 1.3
            let values = &expr
                .iter()
                .map(|e| e.evaluate(&batch))
                .map(|r| r.map(|v| v.into_array(batch.num_rows())))
                .collect::<Result<Vec<_>>>()?;

            // 1.4
            let size_pre = accum.size();
            let res = match mode {
                AggregateMode::Partial | AggregateMode::Single => {
                    accum.update_batch(values)
                }
                AggregateMode::Final | AggregateMode::FinalPartitioned => {
                    accum.merge_batch(values)
                }
            };
            let size_post = accum.size();
            allocated += size_post.saturating_sub(size_pre);
            res
        })?;

    Ok(allocated)
}
