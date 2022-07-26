use arrow::array::ArrayRef;

use self::sum::SumAccumulator;
use super::ExecutorError;
use crate::binder::{AggFunc, BoundExpr};
use crate::types::ScalarValue;

pub mod simple_agg;
mod sum;

/// An accumulator represents a stateful object that lives throughout the evaluation of multiple
/// rows and generically accumulates values.
pub trait Accumulator: Send + Sync {
    /// updates the accumulator's state from a vector of arrays.
    fn update_batch(&mut self, array: &ArrayRef) -> Result<(), ExecutorError>;

    /// returns its value based on its current state.
    fn evaluate(&self) -> Result<ScalarValue, ExecutorError>;
}

fn create_accumulator(expr: &BoundExpr) -> Box<dyn Accumulator> {
    if let BoundExpr::AggFunc(agg_expr) = expr {
        match agg_expr.func {
            AggFunc::Count => todo!(),
            AggFunc::Sum => Box::new(SumAccumulator::new(agg_expr.return_type.clone())),
            AggFunc::Min => todo!(),
            AggFunc::Max => todo!(),
        }
    } else {
        unreachable!(
            "create_accumulator called with non-aggregate expression {:?}",
            expr
        );
    }
}

fn create_accumulators(exprs: &[BoundExpr]) -> Vec<Box<dyn Accumulator>> {
    exprs.iter().map(create_accumulator).collect()
}
