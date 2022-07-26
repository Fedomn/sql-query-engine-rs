mod aggregate;
mod array_compute;
mod evaluator;
mod filter;
mod project;
mod table_scan;

use array_compute::*;
use arrow::error::ArrowError;
use arrow::record_batch::RecordBatch;
use futures::stream::BoxStream;
use futures::TryStreamExt;
use futures_async_stream::try_stream;

use self::aggregate::simple_agg::SimpleAggExecutor;
use self::filter::FilterExecutor;
use self::project::ProjectExecutor;
use self::table_scan::TableScanExecutor;
use crate::optimizer::{
    PhysicalFilter, PhysicalProject, PhysicalSimpleAgg, PhysicalTableScan, PlanRef, PlanTreeNode,
    PlanVisitor,
};
use crate::storage::{StorageError, StorageImpl};

pub type BoxedExecutor = BoxStream<'static, Result<RecordBatch, ExecutorError>>;

pub struct ExecutorBuilder {
    storage: StorageImpl,
}

impl ExecutorBuilder {
    pub fn new(storage: StorageImpl) -> Self {
        Self { storage }
    }

    pub fn build(&mut self, plan: PlanRef) -> BoxedExecutor {
        self.visit(plan).unwrap()
    }

    #[allow(dead_code)]
    pub fn try_collect(
        &mut self,
        plan: PlanRef,
    ) -> BoxStream<'static, Result<RecordBatch, ExecutorError>> {
        self.visit(plan).unwrap()
    }
}

pub async fn try_collect(mut executor: BoxedExecutor) -> Result<Vec<RecordBatch>, ExecutorError> {
    let mut output = Vec::new();
    while let Some(batch) = executor.try_next().await? {
        output.push(batch);
    }
    Ok(output)
}

/// The error type of execution.
#[derive(thiserror::Error, Debug)]
pub enum ExecutorError {
    #[error("storage error: {0}")]
    Storage(
        #[from]
        #[backtrace]
        #[source]
        StorageError,
    ),
    #[error("arrow error: {0}")]
    Arrow(
        #[from]
        #[backtrace]
        #[source]
        ArrowError,
    ),
}

impl PlanVisitor<BoxedExecutor> for ExecutorBuilder {
    fn visit_physical_table_scan(&mut self, plan: &PhysicalTableScan) -> Option<BoxedExecutor> {
        Some(match &self.storage {
            StorageImpl::CsvStorage(storage) => TableScanExecutor {
                plan: plan.clone(),
                storage: storage.clone(),
            }
            .execute(),
            StorageImpl::InMemoryStorage(storage) => TableScanExecutor {
                plan: plan.clone(),
                storage: storage.clone(),
            }
            .execute(),
        })
    }

    fn visit_physical_project(&mut self, plan: &PhysicalProject) -> Option<BoxedExecutor> {
        Some(
            ProjectExecutor {
                exprs: plan.logical().exprs(),
                child: self
                    .visit(plan.children().first().unwrap().clone())
                    .unwrap(),
            }
            .execute(),
        )
    }

    fn visit_physical_filter(&mut self, plan: &PhysicalFilter) -> Option<BoxedExecutor> {
        Some(
            FilterExecutor {
                expr: plan.logical().expr(),
                child: self
                    .visit(plan.children().first().unwrap().clone())
                    .unwrap(),
            }
            .execute(),
        )
    }

    fn visit_physical_simple_agg(&mut self, plan: &PhysicalSimpleAgg) -> Option<BoxedExecutor> {
        Some(
            SimpleAggExecutor {
                agg_funcs: plan.logical().agg_funcs(),
                child: self
                    .visit(plan.children().first().unwrap().clone())
                    .unwrap(),
            }
            .execute(),
        )
    }
}

#[cfg(test)]
mod executor_test {
    use std::sync::Arc;

    use anyhow::Result;
    use arrow::array::{Int64Array, StringArray};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;

    use super::BoxedExecutor;
    use crate::binder::Binder;
    use crate::executor::{try_collect, ExecutorBuilder};
    use crate::optimizer::{InputRefRewriter, PhysicalRewriter, PlanRewriter};
    use crate::parser::parse;
    use crate::planner::Planner;
    use crate::storage::{InMemoryStorage, Storage, StorageError, StorageImpl};
    use crate::util::pretty_batches;

    fn build_record_batch() -> Result<Vec<RecordBatch>, StorageError> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("first_name", DataType::Utf8, false),
            Field::new("last_name", DataType::Utf8, false),
            Field::new("salary", DataType::Int64, false),
        ]));

        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int64Array::from(vec![1, 2, 3, 4])),
                Arc::new(StringArray::from(vec!["Bill", "Gregg", "John", "Von"])),
                Arc::new(StringArray::from(vec![
                    "Hopkins", "Langford", "Travis", "Mill",
                ])),
                Arc::new(Int64Array::from(vec![100, 100, 200, 400])),
            ],
        )?;
        Ok(vec![batch])
    }

    fn build_executor(storage: InMemoryStorage, sql: &str) -> Result<BoxedExecutor> {
        // parse sql to AST
        let stmts = parse(sql).unwrap();

        // bind AST to bound stmts
        let catalog = storage.get_catalog();
        let mut binder = Binder::new(Arc::new(catalog));
        let bound_stmt = binder.bind(&stmts[0]).unwrap();
        println!("bound_stmt = {:#?}", bound_stmt);

        // convert bound stmts to logical plan
        let planner = Planner {};
        let logical_plan = planner.plan(bound_stmt)?;
        println!("logical_plan = {:#?}", logical_plan);
        let mut input_ref_rewriter = InputRefRewriter::default();
        let new_logical_plan = input_ref_rewriter.rewrite(logical_plan);
        println!("new_logical_plan = {:#?}", new_logical_plan);

        // rewrite logical plan to physical plan
        let mut physical_rewriter = PhysicalRewriter {};
        let physical_plan = physical_rewriter.rewrite(new_logical_plan);
        println!("physical_plan = {:#?}", physical_plan);

        // build executor
        let mut builder = ExecutorBuilder::new(StorageImpl::InMemoryStorage(Arc::new(storage)));
        Ok(builder.build(physical_plan))
    }

    #[tokio::test]
    async fn test_executor_works() -> Result<()> {
        // create in-memory storage
        let id = "employee".to_string();
        let storage = InMemoryStorage::new();
        storage.create_mem_table(id.clone(), build_record_batch()?)?;

        // build executor
        let executor = build_executor(storage, "select first_name from employee where id = 1")?;

        // collect result
        let output = try_collect(executor).await?;
        pretty_batches(&output);
        let a = output[0]
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(*a, StringArray::from(vec!["Bill"]));
        Ok(())
    }

    #[tokio::test]
    async fn test_executor_simple_agg_works() -> Result<()> {
        // create in-memory storage
        let id = "employee".to_string();
        let storage = InMemoryStorage::new();
        storage.create_mem_table(id.clone(), build_record_batch()?)?;

        // build executor
        let executor = build_executor(storage, "select sum(salary) from employee")?;

        // collect result
        let output = try_collect(executor).await?;
        pretty_batches(&output);
        let a = output[0]
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        assert_eq!(*a, Int64Array::from(vec![800]));
        Ok(())
    }
}
