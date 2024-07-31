// Copyright 2024 Zinc Labs Inc.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use std::collections::HashMap;

use datafusion::{
    self,
    common::{
        tree_node::{Transformed, TreeNode, TreeNodeRecursion, TreeNodeRewriter, TreeNodeVisitor},
        Column, Result,
    },
    error::DataFusionError,
    optimizer::{optimizer::ApplyOrder, OptimizerConfig, OptimizerRule},
    scalar::ScalarValue,
};
use datafusion_expr::{
    expr::ScalarFunction, expr_rewriter::rewrite_preserving_name, utils::disjunction, BinaryExpr,
    Expr, LogicalPlan, Operator,
};

use crate::service::search::datafusion::udf::match_all_udf::{
    MATCH_ALL_RAW_IGNORE_CASE_UDF_NAME, MATCH_ALL_RAW_UDF_NAME, MATCH_ALL_UDF_NAME,
};

/// Optimization rule that rewrite match_all() to str_match()
#[derive(Default)]
pub struct RewriteMatch {
    #[allow(dead_code)]
    fields: HashMap<String, Vec<String>>,
}

impl RewriteMatch {
    #[allow(missing_docs)]
    pub fn new(fields: HashMap<String, Vec<String>>) -> Self {
        Self { fields }
    }
}

impl OptimizerRule for RewriteMatch {
    fn name(&self) -> &str {
        "rewrite_match"
    }

    fn apply_order(&self) -> Option<ApplyOrder> {
        Some(ApplyOrder::BottomUp)
    }

    fn supports_rewrite(&self) -> bool {
        true
    }

    fn rewrite(
        &self,
        plan: LogicalPlan,
        _config: &dyn OptimizerConfig,
    ) -> Result<Transformed<LogicalPlan>> {
        match plan {
            LogicalPlan::Filter(_) => {
                if plan
                    .expressions()
                    .iter()
                    .map(|expr| expr.exists(|expr| Ok(is_match_all(expr))).unwrap())
                    .any(|x| x)
                {
                    let name = get_table_name(&plan);
                    let fields = self.fields.get(&name).unwrap().clone();
                    let mut expr_rewriter = MatchToFullTextMatch { fields };
                    plan.map_expressions(|expr| {
                        let new_expr = rewrite_preserving_name(expr, &mut expr_rewriter)?;
                        Ok(Transformed::yes(new_expr))
                    })
                } else {
                    Ok(Transformed::no(plan))
                }
            }
            _ => Ok(Transformed::no(plan)),
        }
    }
}

fn is_match_all(expr: &Expr) -> bool {
    match expr {
        Expr::ScalarFunction(ScalarFunction { func, .. }) => {
            func.name() == MATCH_ALL_UDF_NAME
                || func.name() == MATCH_ALL_RAW_IGNORE_CASE_UDF_NAME
                || func.name() == MATCH_ALL_RAW_UDF_NAME
        }
        _ => false,
    }
}

// get table name from logical plan
fn get_table_name(plan: &LogicalPlan) -> String {
    let mut visitor = TableNameVisitor::new();
    plan.visit(&mut visitor).unwrap();
    strip_prefix(visitor.name)
}

struct TableNameVisitor {
    name: String,
}

impl TableNameVisitor {
    pub fn new() -> Self {
        Self {
            name: "".to_string(),
        }
    }
}

impl<'n> TreeNodeVisitor<'n> for TableNameVisitor {
    type Node = LogicalPlan;

    fn f_up(&mut self, plan: &'n LogicalPlan) -> Result<TreeNodeRecursion> {
        match plan {
            LogicalPlan::TableScan(scan) => {
                self.name = scan.table_name.to_string();
                Ok(TreeNodeRecursion::Stop)
            }
            _ => Ok(TreeNodeRecursion::Continue),
        }
    }
}

// strip the catalog and schema prefix
fn strip_prefix(name: String) -> String {
    name.split('.').last().unwrap().to_string()
}

// Rewriter for match_all() to str_match()
#[derive(Debug, Clone)]
pub struct MatchToFullTextMatch {
    #[allow(dead_code)]
    fields: Vec<String>,
}

impl MatchToFullTextMatch {
    pub fn new(fields: Vec<String>) -> Self {
        Self { fields }
    }
}

impl TreeNodeRewriter for MatchToFullTextMatch {
    type Node = Expr;

    fn f_up(&mut self, expr: Expr) -> Result<Transformed<Expr>, DataFusionError> {
        match &expr {
            Expr::ScalarFunction(ScalarFunction { func, args }) => {
                let name = func.name();
                if name == MATCH_ALL_UDF_NAME
                    || name == MATCH_ALL_RAW_IGNORE_CASE_UDF_NAME
                    || name == MATCH_ALL_RAW_UDF_NAME
                {
                    let Expr::Literal(ScalarValue::Utf8(Some(item))) = args[0].clone() else {
                        return Err(DataFusionError::Internal(format!(
                            "Unexpected argument type for match_all() function: {:?}",
                            args[0]
                        )));
                    };
                    let operator = if name == MATCH_ALL_RAW_UDF_NAME {
                        Operator::LikeMatch
                    } else {
                        Operator::ILikeMatch
                    };
                    let mut expr_list = Vec::with_capacity(self.fields.len());
                    let item = Expr::Literal(ScalarValue::Utf8(Some(format!("%{item}%"))));
                    for field in self.fields.iter() {
                        let new_expr = Expr::BinaryExpr(BinaryExpr {
                            left: Box::new(Expr::Column(Column::new_unqualified(field))),
                            op: operator,
                            right: Box::new(item.clone()),
                        });
                        expr_list.push(new_expr);
                    }
                    let new_expr = disjunction(expr_list).unwrap();
                    Ok(Transformed::yes(new_expr))
                } else {
                    Ok(Transformed::no(expr))
                }
            }
            _ => Ok(Transformed::no(expr)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use arrow::array::{Int64Array, StringArray};
    use arrow_schema::DataType;
    use datafusion::{
        arrow::{
            datatypes::{Field, Schema},
            record_batch::RecordBatch,
        },
        assert_batches_eq,
        datasource::MemTable,
        execution::{
            context::SessionState,
            runtime_env::{RuntimeConfig, RuntimeEnv},
        },
        prelude::{SessionConfig, SessionContext},
    };

    use crate::service::search::datafusion::{
        optimizer::rewrite_match::RewriteMatch, udf::match_all_udf,
    };

    #[tokio::test]
    async fn test_rewrite_match() {
        let sqls = [
            (
                "select * from t where match_all('open')",
                vec![
                    "+------------+-------------+-------------+",
                    "| _timestamp | name        | log         |",
                    "+------------+-------------+-------------+",
                    "| 1          | open        | o2          |",
                    "| 3          | openobserve | openobserve |",
                    "+------------+-------------+-------------+",
                ],
            ),
            (
                "select _timestamp from t where match_all('open')",
                vec![
                    "+------------+",
                    "| _timestamp |",
                    "+------------+",
                    "| 1          |",
                    "| 3          |",
                    "+------------+",
                ],
            ),
            (
                "select _timestamp from t where match_all_raw_ignore_case('observe')",
                vec![
                    "+------------+",
                    "| _timestamp |",
                    "+------------+",
                    "| 2          |",
                    "| 3          |",
                    "| 4          |",
                    "+------------+",
                ],
            ),
        ];

        // define a schema.
        let schema = Arc::new(Schema::new(vec![
            Field::new("_timestamp", DataType::Int64, false),
            Field::new("name", DataType::Utf8, false),
            Field::new("log", DataType::Utf8, false),
        ]));

        // define data.
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(vec![1, 2, 3, 4, 5])),
                Arc::new(StringArray::from(vec![
                    "open",
                    "observe",
                    "openobserve",
                    "OBserve",
                    "oo",
                ])),
                Arc::new(StringArray::from(vec![
                    "o2",
                    "obSERVE",
                    "openobserve",
                    "o2",
                    "oo",
                ])),
            ],
        )
        .unwrap();

        let mut fields = HashMap::new();
        fields.insert("t".to_string(), vec!["name".to_string(), "log".to_string()]);
        let state = SessionState::new_with_config_rt(
            SessionConfig::new(),
            Arc::new(RuntimeEnv::new(RuntimeConfig::default()).unwrap()),
        )
        .with_optimizer_rules(vec![
            Arc::new(RewriteMatch::new(fields.clone())),
            // Arc::new(PushDownFilter::new()),
        ]);
        let ctx = SessionContext::new_with_state(state);
        let provider = MemTable::try_new(schema, vec![vec![batch]]).unwrap();
        ctx.register_table("t", Arc::new(provider)).unwrap();
        ctx.register_udf(match_all_udf::MATCH_ALL_RAW_UDF.clone());
        ctx.register_udf(match_all_udf::MATCH_ALL_UDF.clone());
        ctx.register_udf(match_all_udf::MATCH_ALL_RAW_IGNORE_CASE_UDF.clone());

        for item in sqls {
            let df = ctx.sql(item.0).await.unwrap();
            let data = df.collect().await.unwrap();
            assert_batches_eq!(item.1, &data);
        }
    }
}
