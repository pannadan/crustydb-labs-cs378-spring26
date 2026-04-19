use super::OpIterator;
use crate::Managers;
use common::bytecode_expr::ByteCodeExpr;
use common::datatypes::f_decimal;
use common::{AggOp, CrustyError, Field, TableSchema, Tuple};
use std::cmp::{max, min};
use std::collections::HashMap;

/// Aggregate operator.
pub struct Aggregate {
    // Static objects (No need to reset on close)
    managers: &'static Managers,

    // Parameters (No need to reset on close)
    /// Output schema of the form [groupby_field attributes ..., agg_field attributes ...]).
    schema: TableSchema,
    /// Group by fields
    groupby_expr: Vec<ByteCodeExpr>,
    /// Aggregated fields.
    agg_expr: Vec<ByteCodeExpr>,
    /// Aggregation operations.
    ops: Vec<AggOp>,
    /// Child operator to get the data from.
    child: Box<dyn OpIterator>,
    /// If true, then the operator will be rewinded in the future.
    will_rewind: bool,

    // States (Need to reset on close)
    open: bool,
    /// Maps group key -> internal aggregate state.
    map: HashMap<Vec<Field>, Vec<Field>>,
    /// Internal ops where each AVG becomes [AVG, COUNT].
    ops_other: Vec<AggOp>,
    /// Materialized results for iteration.
    results: Vec<Tuple>,
    /// Current index into results.
    index: usize,
}

impl Aggregate {
    pub fn new(
        managers: &'static Managers,
        groupby_expr: Vec<ByteCodeExpr>,
        agg_expr: Vec<ByteCodeExpr>,
        ops: Vec<AggOp>,
        schema: TableSchema,
        child: Box<dyn OpIterator>,
    ) -> Self {
        assert!(ops.len() == agg_expr.len());
        Self {
            managers,
            schema,
            groupby_expr,
            agg_expr,
            ops,
            child,
            will_rewind: false,
            open: false,
            map: HashMap::new(),
            ops_other: Vec::new(),
            results: Vec::new(),
            index: 0,
        }
    }

    fn merge_fields(op: AggOp, field_val: &Field, acc: &mut Field) -> Result<(), CrustyError> {
        match op {
            AggOp::Count => *acc = (acc.clone() + Field::Int(1))?,
            AggOp::Max => {
                *acc = max(acc.clone(), field_val.clone());
            }
            AggOp::Min => {
                *acc = min(acc.clone(), field_val.clone());
            }
            AggOp::Sum => {
                *acc = (acc.clone() + field_val.clone())?;
            }
            AggOp::Avg => {
                *acc = (acc.clone() + field_val.clone())?;
            }
        }
        Ok(())
    }

    fn place_field(op: AggOp, field_val: &Field) -> Field {
        match op {
            AggOp::Count => Field::Int(1),
            AggOp::Max | AggOp::Min | AggOp::Sum => field_val.clone(),
            AggOp::Avg => match field_val {
                Field::Int(v) => f_decimal(*v as f64),
                Field::Decimal(..) => field_val.clone(),
                _ => field_val.clone(),
            },
        }
    }

    fn build_internal_ops(&mut self) {
        self.ops_other.clear();
        for op in &self.ops {
            match op {
                AggOp::Avg => {
                    self.ops_other.push(AggOp::Avg);
                    self.ops_other.push(AggOp::Count);
                }
                _ => self.ops_other.push(*op),
            }
        }
    }

    fn eval_groupby_fields(&self, tuple: &Tuple) -> Vec<Field> {
        self.groupby_expr.iter().map(|expr| expr.eval(tuple)).collect()
    }

    fn eval_internal_agg_fields(&self, tuple: &Tuple) -> Vec<Field> {
        let mut fields = Vec::new();

        for (expr, op) in self.agg_expr.iter().zip(self.ops.iter()) {
            let field = expr.eval(tuple);
            match op {
                AggOp::Avg => {
                    let sum_field = match field.clone() {
                        Field::Int(v) => f_decimal(v as f64),
                        other => other,
                    };
                    fields.push(sum_field);
                    fields.push(Field::Int(1));
                }
                _ => fields.push(field),
            }
        }

        fields
    }

    pub fn merge_tuple_into_group(&mut self, tuple: &Tuple) {
        let groupby_fields = self.eval_groupby_fields(tuple);
        let agg_fields = self.eval_internal_agg_fields(tuple);

        if let Some(fields) = self.map.get_mut(&groupby_fields) {
            for i in 0..fields.len() {
                Self::merge_fields(self.ops_other[i], &agg_fields[i], &mut fields[i]).unwrap();
            }
        } else {
            let mut init_fields = Vec::with_capacity(agg_fields.len());
            for i in 0..agg_fields.len() {
                init_fields.push(Self::place_field(self.ops_other[i], &agg_fields[i]));
            }
            self.map.insert(groupby_fields, init_fields);
        }
    }

    fn materialize_results(&mut self) -> Result<(), CrustyError> {
        self.results.clear();

        let mut entries: Vec<(Vec<Field>, Vec<Field>)> = self.map.drain().collect();
        entries.sort_by(|(a, _), (b, _)| a.cmp(b));

        for (group_key, agg_vals) in entries {
            let mut out_fields = group_key;
            let mut internal_idx = 0;

            for op in &self.ops {
                match op {
                    AggOp::Avg => {
                        let sum = agg_vals[internal_idx].clone();
                        let count = agg_vals[internal_idx + 1].clone();
                        let avg = (sum / count)?;
                        out_fields.push(avg);
                        internal_idx += 2;
                    }
                    _ => {
                        out_fields.push(agg_vals[internal_idx].clone());
                        internal_idx += 1;
                    }
                }
            }

            self.results.push(Tuple::new(out_fields));
        }

        Ok(())
    }
}

impl OpIterator for Aggregate {
    fn configure(&mut self, will_rewind: bool) {
        self.will_rewind = will_rewind;
        self.child.configure(false);
    }

    fn open(&mut self) -> Result<(), CrustyError> {
        if !self.open {
            self.child.open()?;
            self.open = true;
            self.index = 0;
            self.map.clear();
            self.results.clear();

            self.build_internal_ops();

            while let Some(tuple) = self.child.next()? {
                self.merge_tuple_into_group(&tuple);
            }

            self.materialize_results()?;
        }
        Ok(())
    }

    fn next(&mut self) -> Result<Option<Tuple>, CrustyError> {
        if !self.open {
            panic!("Operator has not been opened")
        }

        if self.index < self.results.len() {
            let t = self.results[self.index].clone();
            self.index += 1;
            Ok(Some(t))
        } else {
            Ok(None)
        }
    }

    fn close(&mut self) -> Result<(), CrustyError> {
        self.child.close()?;
        self.open = false;
        self.index = 0;
        self.map.clear();
        self.ops_other.clear();
        self.results.clear();
        let _ = self.managers;
        Ok(())
    }

    fn rewind(&mut self) -> Result<(), CrustyError> {
        if !self.open {
            panic!("Operator has not been opened")
        }

        self.index = 0;
        Ok(())
    }

    fn get_schema(&self) -> &TableSchema {
        &self.schema
    }
}

#[cfg(test)]
mod test {
    use super::super::TupleIterator;
    use super::*;
    use crate::testutil::{execute_iter, new_test_managers, TestTuples};
    use common::{
        bytecode_expr::colidx_expr,
        datatypes::{f_int, f_str},
    };

    fn get_iter(
        groupby_expr: Vec<ByteCodeExpr>,
        agg_expr: Vec<ByteCodeExpr>,
        ops: Vec<AggOp>,
    ) -> Box<dyn OpIterator> {
        let setup = TestTuples::new("");
        let managers = new_test_managers();
        let dummy_schema = TableSchema::new(vec![]);
        let mut iter = Box::new(Aggregate::new(
            managers,
            groupby_expr,
            agg_expr,
            ops,
            dummy_schema,
            Box::new(TupleIterator::new(
                setup.tuples.clone(),
                setup.schema.clone(),
            )),
        ));
        iter.configure(false);
        iter
    }

    fn run_aggregate(
        groupby_expr: Vec<ByteCodeExpr>,
        agg_expr: Vec<ByteCodeExpr>,
        ops: Vec<AggOp>,
    ) -> Vec<Tuple> {
        let mut iter = get_iter(groupby_expr, agg_expr, ops);
        execute_iter(&mut *iter, true).unwrap()
    }

    mod aggregation_test {
        use super::*;

        #[test]
        fn test_empty_group() {
            let group_by = vec![];
            let agg = vec![colidx_expr(0), colidx_expr(1), colidx_expr(2)];
            let ops = vec![AggOp::Count, AggOp::Max, AggOp::Avg];
            let t = run_aggregate(group_by, agg, ops);
            assert_eq!(t.len(), 1);
            assert_eq!(t[0], Tuple::new(vec![f_int(6), f_int(2), f_decimal(4.0)]));
        }

        #[test]
        fn test_empty_aggregation() {
            let group_by = vec![colidx_expr(2)];
            let agg = vec![];
            let ops = vec![];
            let t = run_aggregate(group_by, agg, ops);
            assert_eq!(t.len(), 3);
            assert_eq!(t[0], Tuple::new(vec![f_int(3)]));
            assert_eq!(t[1], Tuple::new(vec![f_int(4)]));
            assert_eq!(t[2], Tuple::new(vec![f_int(5)]));
        }

        #[test]
        fn test_count() {
            let group_by = vec![colidx_expr(1), colidx_expr(2)];
            let agg = vec![colidx_expr(0)];
            let ops = vec![AggOp::Count];
            let t = run_aggregate(group_by, agg, ops);
            assert_eq!(t.len(), 4);
            assert_eq!(t[0], Tuple::new(vec![f_int(1), f_int(3), f_int(2)]));
            assert_eq!(t[1], Tuple::new(vec![f_int(1), f_int(4), f_int(1)]));
            assert_eq!(t[2], Tuple::new(vec![f_int(2), f_int(4), f_int(1)]));
            assert_eq!(t[3], Tuple::new(vec![f_int(2), f_int(5), f_int(2)]));
        }

        #[test]
        fn test_sum() {
            let group_by = vec![colidx_expr(1), colidx_expr(2)];
            let agg = vec![colidx_expr(0)];
            let ops = vec![AggOp::Sum];
            let tuples = run_aggregate(group_by, agg, ops);
            assert_eq!(tuples.len(), 4);
            assert_eq!(tuples[0], Tuple::new(vec![f_int(1), f_int(3), f_int(3)]));
            assert_eq!(tuples[1], Tuple::new(vec![f_int(1), f_int(4), f_int(3)]));
            assert_eq!(tuples[2], Tuple::new(vec![f_int(2), f_int(4), f_int(4)]));
            assert_eq!(tuples[3], Tuple::new(vec![f_int(2), f_int(5), f_int(11)]));
        }

        #[test]
        fn test_max() {
            let group_by = vec![colidx_expr(1), colidx_expr(2)];
            let agg = vec![colidx_expr(3)];
            let ops = vec![AggOp::Max];
            let t = run_aggregate(group_by, agg, ops);
            assert_eq!(t.len(), 4);
            assert_eq!(t[0], Tuple::new(vec![f_int(1), f_int(3), f_str("G")]));
            assert_eq!(t[1], Tuple::new(vec![f_int(1), f_int(4), f_str("A")]));
            assert_eq!(t[2], Tuple::new(vec![f_int(2), f_int(4), f_str("G")]));
            assert_eq!(t[3], Tuple::new(vec![f_int(2), f_int(5), f_str("G")]));
        }

        #[test]
        fn test_min() {
            let group_by = vec![colidx_expr(1), colidx_expr(2)];
            let agg = vec![colidx_expr(3)];
            let ops = vec![AggOp::Min];
            let t = run_aggregate(group_by, agg, ops);
            assert!(t.len() == 4);
            assert_eq!(t[0], Tuple::new(vec![f_int(1), f_int(3), f_str("E")]));
            assert_eq!(t[1], Tuple::new(vec![f_int(1), f_int(4), f_str("A")]));
            assert_eq!(t[2], Tuple::new(vec![f_int(2), f_int(4), f_str("G")]));
            assert_eq!(t[3], Tuple::new(vec![f_int(2), f_int(5), f_str("G")]));
        }

        #[test]
        fn test_avg() {
            let group_by = vec![colidx_expr(1), colidx_expr(2)];
            let agg = vec![colidx_expr(0)];
            let ops = vec![AggOp::Avg];
            let t = run_aggregate(group_by, agg, ops);
            assert_eq!(t.len(), 4);
            assert_eq!(t[0], Tuple::new(vec![f_int(1), f_int(3), f_decimal(1.5)]));
            assert_eq!(t[1], Tuple::new(vec![f_int(1), f_int(4), f_decimal(3.0)]));
            assert_eq!(t[2], Tuple::new(vec![f_int(2), f_int(4), f_decimal(4.0)]));
            assert_eq!(t[3], Tuple::new(vec![f_int(2), f_int(5), f_decimal(5.5)]));
        }

        #[test]
        fn test_multi_column_aggregation() {
            let group_by = vec![colidx_expr(3)];
            let agg = vec![colidx_expr(0), colidx_expr(1), colidx_expr(2)];
            let ops = vec![AggOp::Count, AggOp::Max, AggOp::Avg];
            let t = run_aggregate(group_by, agg, ops);
            assert_eq!(t.len(), 3);
            assert_eq!(
                t[0],
                Tuple::new(vec![f_str("A"), f_int(1), f_int(1), f_decimal(4.0)])
            );
            assert_eq!(
                t[1],
                Tuple::new(vec![f_str("E"), f_int(1), f_int(1), f_decimal(3.0)])
            );
            assert_eq!(
                t[2],
                Tuple::new(vec![f_str("G"), f_int(4), f_int(2), f_decimal(4.25)])
            );
        }

        #[test]
        #[should_panic]
        fn test_merge_tuples_not_int() {
            let group_by = vec![];
            let agg = vec![colidx_expr(3)];
            let ops = vec![AggOp::Avg];
            let _ = run_aggregate(group_by, agg, ops);
        }
    }

    mod opiterator_test {
        use super::*;

        #[test]
        #[should_panic]
        fn test_next_not_open() {
            let mut iter = get_iter(vec![], vec![], vec![]);
            let _ = iter.next();
        }

        #[test]
        #[should_panic]
        fn test_rewind_not_open() {
            let mut iter = get_iter(vec![], vec![], vec![]);
            let _ = iter.rewind();
        }

        #[test]
        fn test_open() {
            let mut iter = get_iter(vec![], vec![], vec![]);
            iter.open().unwrap();
        }

        #[test]
        fn test_close() {
            let mut iter = get_iter(vec![], vec![], vec![]);
            iter.open().unwrap();
            iter.close().unwrap();
        }

        #[test]
        fn test_rewind() {
            let mut iter = get_iter(vec![colidx_expr(2)], vec![colidx_expr(0)], vec![AggOp::Max]);
            iter.configure(true);
            let t_before = execute_iter(&mut *iter, true).unwrap();
            iter.rewind().unwrap();
            let t_after = execute_iter(&mut *iter, true).unwrap();
            assert_eq!(t_before, t_after);
        }
    }
}