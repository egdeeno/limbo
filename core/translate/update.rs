use std::sync::Arc;

use crate::translate::plan::Operation;
use crate::vdbe::BranchOffset;
use crate::{
    bail_parse_error,
    schema::{Schema, Table},
    util::normalize_ident,
    vdbe::builder::{ProgramBuilder, ProgramBuilderOpts, QueryMode},
    SymbolTable,
};
use limbo_sqlite3_parser::ast::{self, Expr, ResultColumn, SortOrder, Update};

use super::emitter::{emit_program, Resolver};
use super::optimizer::optimize_plan;
use super::plan::{
    Direction, IterationDirection, Plan, ResultSetColumn, TableReference, UpdatePlan,
};
use super::planner::bind_column_references;
use super::planner::{parse_limit, parse_where};

/*
* Update is simple. By default we scan the table, and for each row, we check the WHERE
* clause. If it evaluates to true, we build the new record with the updated value and insert.
*
* EXAMPLE:
*
sqlite> explain update t set a = 100 where b = 5;
addr  opcode         p1    p2    p3    p4             p5  comment
----  -------------  ----  ----  ----  -------------  --  -------------
0     Init           0     16    0                    0   Start at 16
1     Null           0     1     2                    0   r[1..2]=NULL
2     Noop           1     0     1                    0
3     OpenWrite      0     2     0     3              0   root=2 iDb=0; t
4     Rewind         0     15    0                    0
5       Column         0     1     6                    0   r[6]= cursor 0 column 1
6       Ne             7     14    6     BINARY-8       81  if r[6]!=r[7] goto 14
7       Rowid          0     2     0                    0   r[2]= rowid of 0
8       IsNull         2     15    0                    0   if r[2]==NULL goto 15
9       Integer        100   3     0                    0   r[3]=100
10      Column         0     1     4                    0   r[4]= cursor 0 column 1
11      Column         0     2     5                    0   r[5]= cursor 0 column 2
12      MakeRecord     3     3     1                    0   r[1]=mkrec(r[3..5])
13      Insert         0     1     2     t              7   intkey=r[2] data=r[1]
14    Next           0     5     0                    1
15    Halt           0     0     0                    0
16    Transaction    0     1     1     0              1   usesStmtJournal=0
17    Integer        5     7     0                    0   r[7]=5
18    Goto           0     1     0                    0
*/
pub fn translate_update(
    query_mode: QueryMode,
    schema: &Schema,
    body: &mut Update,
    syms: &SymbolTable,
) -> crate::Result<ProgramBuilder> {
    let mut plan = prepare_update_plan(schema, body)?;
    optimize_plan(&mut plan, schema)?;
    let resolver = Resolver::new(syms);
    // TODO: freestyling these numbers
    let mut program = ProgramBuilder::new(ProgramBuilderOpts {
        query_mode,
        num_cursors: 1,
        approx_num_insns: 20,
        approx_num_labels: 4,
    });
    emit_program(&mut program, plan, syms)?;
    Ok(program)
}

pub fn prepare_update_plan(schema: &Schema, body: &mut Update) -> crate::Result<Plan> {
    if body.with.is_some() {
        bail_parse_error!("WITH clause is not supported");
    }
    if body.or_conflict.is_some() {
        bail_parse_error!("ON CONFLICT clause is not supported");
    }
    let table_name = &body.tbl_name.name;
    let table = match schema.get_table(table_name.0.as_str()) {
        Some(table) => table,
        None => bail_parse_error!("Parse error: no such table: {}", table_name),
    };
    let Some(btree_table) = table.btree() else {
        bail_parse_error!("Error: {} is not a btree table", table_name);
    };
    let iter_dir = body
        .order_by
        .as_ref()
        .and_then(|order_by| {
            order_by.first().and_then(|ob| {
                ob.order.map(|o| match o {
                    SortOrder::Asc => IterationDirection::Forwards,
                    SortOrder::Desc => IterationDirection::Backwards,
                })
            })
        })
        .unwrap_or(IterationDirection::Forwards);
    let table_references = vec![TableReference {
        table: match table.as_ref() {
            Table::Virtual(vtab) => Table::Virtual(vtab.clone()),
            Table::BTree(btree_table) => Table::BTree(btree_table.clone()),
            _ => unreachable!(),
        },
        identifier: table_name.0.clone(),
        op: Operation::Scan {
            iter_dir,
            index: None,
        },
        join_info: None,
    }];
    let set_clauses = body
        .sets
        .iter_mut()
        .map(|set| {
            let ident = normalize_ident(set.col_names[0].0.as_str());
            let col_index = table
                .columns()
                .iter()
                .enumerate()
                .find_map(|(i, col)| {
                    col.name
                        .as_ref()
                        .filter(|name| name.eq_ignore_ascii_case(&ident))
                        .map(|_| i)
                })
                .ok_or_else(|| {
                    crate::LimboError::ParseError(format!(
                        "column '{}' not found in table '{}'",
                        ident, table_name.0
                    ))
                })?;

            let _ = bind_column_references(&mut set.expr, &table_references, None);
            Ok((col_index, set.expr.clone()))
        })
        .collect::<Result<Vec<(usize, Expr)>, crate::LimboError>>()?;

    let mut where_clause = vec![];
    let mut result_columns = vec![];
    if let Some(returning) = &mut body.returning {
        for rc in returning.iter_mut() {
            if let ResultColumn::Expr(expr, alias) = rc {
                bind_column_references(expr, &table_references, None)?;
                result_columns.push(ResultSetColumn {
                    expr: expr.clone(),
                    alias: alias.as_ref().and_then(|a| {
                        if let ast::As::As(name) = a {
                            Some(name.to_string())
                        } else {
                            None
                        }
                    }),
                    contains_aggregates: false,
                });
            } else {
                bail_parse_error!("Only expressions are allowed in RETURNING clause");
            }
        }
    }
    let order_by = body.order_by.as_ref().map(|order| {
        order
            .iter()
            .map(|o| {
                (
                    o.expr.clone(),
                    o.order
                        .map(|s| match s {
                            SortOrder::Asc => Direction::Ascending,
                            SortOrder::Desc => Direction::Descending,
                        })
                        .unwrap_or(Direction::Ascending),
                )
            })
            .collect()
    });
    // Parse the WHERE clause
    parse_where(
        body.where_clause.as_ref().map(|w| *w.clone()),
        &table_references,
        Some(&result_columns),
        &mut where_clause,
    )?;

    // Parse the LIMIT/OFFSET clause
    let (limit, offset) = body
        .limit
        .as_ref()
        .map(|l| parse_limit(l))
        .unwrap_or(Ok((None, None)))?;

    Ok(Plan::Update(UpdatePlan {
        table_references,
        set_clauses,
        where_clause,
        returning: Some(result_columns),
        order_by,
        limit,
        offset,
        contains_constant_false_condition: false,
    }))
}

// fn translate_vtab_update(
//     mut program: ProgramBuilder,
//     body: &mut Update,
//     table: Arc<Table>,
//     resolver: &Resolver,
// ) -> crate::Result<ProgramBuilder> {
//     let start_label = program.allocate_label();
//     program.emit_insn(Insn::Init {
//         target_pc: start_label,
//     });
//     let start_offset = program.offset();
//     let vtab = table.virtual_table().unwrap();
//     let cursor_id = program.alloc_cursor_id(
//         Some(table.get_name().to_string()),
//         CursorType::VirtualTable(vtab.clone()),
//     );
//     let referenced_tables = vec![TableReference {
//         table: Table::Virtual(table.virtual_table().unwrap().clone()),
//         identifier: table.get_name().to_string(),
//         op: Operation::Scan { iter_dir: None },
//         join_info: None,
//     }];
//     program.emit_insn(Insn::VOpenAsync { cursor_id });
//     program.emit_insn(Insn::VOpenAwait {});
//
//     let argv_start = program.alloc_registers(0);
//     let end_label = program.allocate_label();
//     let skip_label = program.allocate_label();
//     program.emit_insn(Insn::VFilter {
//         cursor_id,
//         pc_if_empty: end_label,
//         args_reg: argv_start,
//         arg_count: 0,
//     });
//
//     let loop_start = program.offset();
//     let start_reg = program.alloc_registers(2 + table.columns().len());
//     let old_rowid = start_reg;
//     let new_rowid = start_reg + 1;
//     let column_regs = start_reg + 2;
//
//     program.emit_insn(Insn::RowId {
//         cursor_id,
//         dest: old_rowid,
//     });
//     program.emit_insn(Insn::RowId {
//         cursor_id,
//         dest: new_rowid,
//     });
//
//     for (i, _) in table.columns().iter().enumerate() {
//         let dest = column_regs + i;
//         program.emit_insn(Insn::VColumn {
//             cursor_id,
//             column: i,
//             dest,
//         });
//     }
//
//     if let Some(ref mut where_clause) = body.where_clause {
//         bind_column_references(where_clause, &referenced_tables, None)?;
//         translate_condition_expr(
//             &mut program,
//             &referenced_tables,
//             where_clause,
//             ConditionMetadata {
//                 jump_if_condition_is_true: false,
//                 jump_target_when_true: BranchOffset::Placeholder,
//                 jump_target_when_false: skip_label,
//             },
//             resolver,
//         )?;
//     }
//     // prepare updated columns in place
//     for expr in body.sets.iter() {
//         let Some(col_index) = table.columns().iter().position(|t| {
//             t.name
//                 .as_ref()
//                 .unwrap()
//                 .eq_ignore_ascii_case(&expr.col_names[0].0)
//         }) else {
//             bail_parse_error!("column {} not found", expr.col_names[0].0);
//         };
//         translate_expr(
//             &mut program,
//             Some(&referenced_tables),
//             &expr.expr,
//             column_regs + col_index,
//             resolver,
//         )?;
//     }
//
//     let arg_count = 2 + table.columns().len();
//     program.emit_insn(Insn::VUpdate {
//         cursor_id,
//         arg_count,
//         start_reg: old_rowid,
//         vtab_ptr: vtab.implementation.ctx as usize,
//         conflict_action: 0,
//     });
//
//     program.resolve_label(skip_label, program.offset());
//     program.emit_insn(Insn::VNext {
//         cursor_id,
//         pc_if_next: loop_start,
//     });
//
//     program.resolve_label(end_label, program.offset());
//     program.emit_insn(Insn::Halt {
//         err_code: 0,
//         description: String::new(),
//     });
//     program.resolve_label(start_label, program.offset());
//     program.emit_insn(Insn::Transaction { write: true });
//
//     program.emit_constant_insns();
//     program.emit_insn(Insn::Goto {
//         target_pc: start_offset,
//     });
//     program.table_references = referenced_tables.clone();
//     Ok(program)
// }
