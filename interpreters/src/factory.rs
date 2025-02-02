// Copyright 2022 CeresDB Project Authors. Licensed under Apache-2.0.

//! Interpreter factory

use catalog::manager::Manager as CatalogManager;
use query_engine::executor::Executor;
use sql::plan::Plan;
use table_engine::engine::TableEngineRef;

use crate::{
    alter_table::AlterTableInterpreter, context::Context, create::CreateInterpreter,
    describe::DescribeInterpreter, drop::DropInterpreter, exists::ExistsInterpreter,
    insert::InsertInterpreter, interpreter::InterpreterPtr, select::SelectInterpreter,
    show_create::ShowCreateInInterpreter,
};

/// A factory to create interpreters
pub struct Factory<Q, C> {
    query_executor: Q,
    catalog_manager: C,
    table_engine: TableEngineRef,
}

impl<Q: Executor + 'static, C: CatalogManager + 'static> Factory<Q, C> {
    pub fn new(query_executor: Q, catalog_manager: C, table_engine: TableEngineRef) -> Self {
        Self {
            query_executor,
            catalog_manager,
            table_engine,
        }
    }

    pub fn create(self, ctx: Context, plan: Plan) -> InterpreterPtr {
        match plan {
            Plan::Query(p) => SelectInterpreter::create(ctx, p, self.query_executor),
            Plan::Insert(p) => InsertInterpreter::create(ctx, p),
            Plan::Create(p) => {
                CreateInterpreter::create(ctx, p, self.catalog_manager, self.table_engine)
            }
            Plan::Drop(p) => {
                DropInterpreter::create(ctx, p, self.catalog_manager, self.table_engine)
            }
            Plan::Describe(p) => DescribeInterpreter::create(p),
            Plan::AlterTable(p) => AlterTableInterpreter::create(p),
            Plan::ShowCreate(p) => ShowCreateInInterpreter::create(p),
            Plan::Exists(p) => ExistsInterpreter::create(p),
        }
    }
}
