use std::sync::Arc;

use super::expr_rewriter::ExprRewriter;
use super::{LogicalAgg, LogicalFilter, LogicalProject, LogicalTableScan, PlanRef, PlanRewriter};
use crate::binder::{BoundColumnRef, BoundExpr, BoundInputRef};

#[derive(Default)]
pub struct InputRefRewriter {
    /// The bound exprs of the last visited plan node, which is used to resolve the index of
    /// RecordBatch.
    bindings: Vec<BoundExpr>,
}

impl InputRefRewriter {
    fn rewrite_internal(&self, expr: &mut BoundExpr) {
        // Find input expr in bindings.
        if let Some(idx) = self.bindings.iter().position(|e| *e == expr.clone()) {
            *expr = BoundExpr::InputRef(BoundInputRef {
                index: idx,
                return_type: expr.return_type().unwrap(),
            });
            return;
        }

        // If not found in bindings, expand nested expr and then continuity rewrite_expr.
        match expr {
            BoundExpr::BinaryOp(e) => {
                self.rewrite_expr(e.left.as_mut());
                self.rewrite_expr(e.right.as_mut());
            }
            BoundExpr::TypeCast(e) => self.rewrite_expr(e.expr.as_mut()),
            BoundExpr::AggFunc(e) => {
                for arg in &mut e.exprs {
                    self.rewrite_expr(arg);
                }
            }
            _ => unreachable!(
                "unexpected expr type {:?} for InputRefRewriter, binding: {:?}",
                expr, self.bindings
            ),
        }
    }
}

impl ExprRewriter for InputRefRewriter {
    fn rewrite_column_ref(&self, expr: &mut BoundExpr) {
        self.rewrite_internal(expr);
    }

    fn rewrite_type_cast(&self, expr: &mut BoundExpr) {
        self.rewrite_internal(expr);
    }

    fn rewrite_binary_op(&self, expr: &mut BoundExpr) {
        self.rewrite_internal(expr);
    }

    fn rewrite_agg_func(&self, expr: &mut BoundExpr) {
        self.rewrite_internal(expr);
    }
}

impl PlanRewriter for InputRefRewriter {
    fn rewrite_logical_table_scan(&mut self, plan: &LogicalTableScan) -> PlanRef {
        self.bindings = plan
            .columns()
            .iter()
            .map(|c| {
                BoundExpr::ColumnRef(BoundColumnRef {
                    column_catalog: c.clone(),
                })
            })
            .collect();
        Arc::new(plan.clone())
    }

    fn rewrite_logical_project(&mut self, plan: &LogicalProject) -> PlanRef {
        let new_child = self.rewrite(plan.input());

        let bindings = plan.exprs();

        let mut new_exprs = plan.exprs();
        for expr in &mut new_exprs {
            self.rewrite_expr(expr);
        }

        self.bindings = bindings;
        let new_plan = LogicalProject::new(new_exprs, new_child);
        Arc::new(new_plan)
    }

    fn rewrite_logical_filter(&mut self, plan: &LogicalFilter) -> PlanRef {
        let new_child = self.rewrite(plan.input());

        let mut new_expr = plan.expr();
        self.rewrite_expr(&mut new_expr);

        let new_plan = LogicalFilter::new(new_expr, new_child);
        Arc::new(new_plan)
    }

    fn rewrite_logical_agg(&mut self, plan: &LogicalAgg) -> PlanRef {
        let new_child = self.rewrite(plan.input());
        let bindings = plan
            .group_by()
            .iter()
            .chain(plan.agg_funcs().iter())
            .cloned()
            .collect();

        let mut new_agg_funcs = plan.agg_funcs();
        for expr in &mut new_agg_funcs {
            self.rewrite_expr(expr);
        }

        let mut new_group_exprs = plan.group_by();
        for expr in &mut new_group_exprs {
            self.rewrite_expr(expr);
        }

        self.bindings = bindings;
        let new_plan = LogicalAgg::new(new_agg_funcs, new_group_exprs, new_child);
        Arc::new(new_plan)
    }
}

#[cfg(test)]
mod input_ref_rewriter_test {
    use arrow::datatypes::DataType;
    use sqlparser::ast::BinaryOperator;

    use super::*;
    use crate::binder::{AggFunc, BoundAggFunc, BoundBinaryOp};
    use crate::catalog::{ColumnCatalog, ColumnDesc};
    use crate::types::ScalarValue;

    fn build_test_column(column_name: String) -> ColumnCatalog {
        ColumnCatalog {
            id: column_name.clone(),
            desc: ColumnDesc {
                name: column_name,
                data_type: DataType::Int32,
            },
        }
    }

    fn build_logical_table_scan() -> LogicalTableScan {
        LogicalTableScan::new(
            "t".to_string(),
            vec![
                build_test_column("c1".to_string()),
                build_test_column("c2".to_string()),
            ],
        )
    }

    fn build_logical_project(input: PlanRef) -> LogicalProject {
        LogicalProject::new(
            vec![BoundExpr::ColumnRef(BoundColumnRef {
                column_catalog: build_test_column("c2".to_string()),
            })],
            input,
        )
    }

    fn build_logical_filter(input: PlanRef) -> LogicalFilter {
        LogicalFilter::new(
            BoundExpr::BinaryOp(BoundBinaryOp {
                op: BinaryOperator::Eq,
                left: Box::new(BoundExpr::ColumnRef(BoundColumnRef {
                    column_catalog: build_test_column("c1".to_string()),
                })),
                right: Box::new(BoundExpr::Constant(ScalarValue::Int32(Some(2)))),
                return_type: Some(DataType::Boolean),
            }),
            input,
        )
    }

    fn build_logical_project_with_simple_agg(input: PlanRef) -> LogicalProject {
        let expr = BoundExpr::AggFunc(BoundAggFunc {
            func: AggFunc::Sum,
            exprs: vec![BoundExpr::ColumnRef(BoundColumnRef {
                column_catalog: build_test_column("c1".to_string()),
            })],
            return_type: DataType::Int32,
        });
        let simple_agg = LogicalAgg::new(vec![expr.clone()], vec![], input);
        LogicalProject::new(vec![expr], Arc::new(simple_agg))
    }

    #[test]
    fn test_rewrite_column_ref_to_input_ref() {
        let plan = build_logical_table_scan();
        let filter_plan = build_logical_filter(Arc::new(plan));
        let project_plan = build_logical_project(Arc::new(filter_plan));

        let mut rewriter = InputRefRewriter::default();
        let new_plan = rewriter.rewrite(Arc::new(project_plan));

        assert_eq!(
            new_plan.as_logical_project().unwrap().exprs(),
            vec![BoundExpr::InputRef(BoundInputRef {
                index: 1,
                return_type: DataType::Int32,
            })]
        );
        assert_eq!(
            new_plan.children()[0].as_logical_filter().unwrap().expr(),
            BoundExpr::BinaryOp(BoundBinaryOp {
                op: BinaryOperator::Eq,
                left: Box::new(BoundExpr::InputRef(BoundInputRef {
                    index: 0,
                    return_type: DataType::Int32,
                })),
                right: Box::new(BoundExpr::Constant(ScalarValue::Int32(Some(2)))),
                return_type: Some(DataType::Boolean),
            })
        );
    }

    #[test]
    fn test_rewrite_simple_aggregation_column_ref_to_input_ref() {
        let plan = build_logical_table_scan();
        let plan = build_logical_project_with_simple_agg(Arc::new(plan));

        let mut rewriter = InputRefRewriter::default();
        let new_plan = rewriter.rewrite(Arc::new(plan));

        assert_eq!(
            new_plan.as_logical_project().unwrap().exprs(),
            vec![BoundExpr::InputRef(BoundInputRef {
                index: 0,
                return_type: DataType::Int32,
            })]
        );
    }
}
