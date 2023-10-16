// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Optimizer implementation for `CREATE MATERIALIZED VIEW` statements.

use std::sync::Arc;

use differential_dataflow::lattice::Lattice;
use maplit::btreemap;
use mz_compute_types::plan::Plan;
use mz_compute_types::ComputeInstanceId;
use mz_expr::{MirRelationExpr, OptimizedMirRelationExpr};
use mz_repr::explain::trace_plan;
use mz_repr::{ColumnName, GlobalId, RelationDesc, Timestamp};
use mz_sql::plan::HirRelationExpr;
use mz_transform::dataflow::DataflowMetainfo;
use mz_transform::normalize_lets::normalize_lets;
use mz_transform::typecheck::{empty_context, SharedContext as TypecheckContext};
use mz_transform::Optimizer as TransformOptimizer;
use timely::progress::Antichain;
use tracing::{span, Level};

use crate::catalog::Catalog;
use crate::coord::dataflows::{ComputeInstanceSnapshot, DataflowBuilder};
use crate::optimize::{
    LirDataflowDescription, MirDataflowDescription, Optimize, OptimizerConfig, OptimizerError,
};
use crate::CollectionIdBundle;

pub struct OptimizeMaterializedView {
    /// A typechecking context to use throughout the optimizer pipeline.
    typecheck_ctx: TypecheckContext,
    /// A snapshot of the catalog state.
    catalog: Arc<Catalog>,
    /// A snapshot of the compute instance that will run the dataflows.
    compute_instance: ComputeInstanceSnapshot,
    /// A durable GlobalId to be used with the exported materialized view sink.
    exported_sink_id: GlobalId,
    /// A transient GlobalId to be used when constructing the dataflow.
    internal_view_id: GlobalId,
    /// The resulting column names.
    column_names: Vec<ColumnName>,
    /// Output columns that are asserted to be not null in the `CREATE VIEW`
    /// statement.
    non_null_assertions: Vec<usize>,
    /// A human-readable name exposed internally (useful for debugging).
    debug_name: String,
    // Optimizer config.
    config: OptimizerConfig,
}

/// The (sealed intermediate) result after HIR ⇒ MIR lowering and decorrelation
/// and MIR optimization.
#[derive(Clone)]
pub struct LocalMirPlan {
    expr: MirRelationExpr,
}

/// The (sealed intermediate) result after:
///
/// 1. embedding a [`LocalMirPlan`] into a [`MirDataflowDescription`],
/// 2. transitively inlining referenced views, and
/// 3. jointly optimizing the `MIR` plans in the [`MirDataflowDescription`].
#[derive(Clone)]
pub struct GlobalMirPlan<T: Clone> {
    df_desc: MirDataflowDescription,
    df_meta: DataflowMetainfo,
    ts_info: T,
}

/// Timestamp information type for [`GlobalMirPlan`] structs representing an
/// optimization result without a resolved timestamp.
#[derive(Clone)]
pub struct Unresolved {
    compute_instance_id: ComputeInstanceId,
}

/// Timestamp information type for [`GlobalMirPlan`] structs representing an
/// optimization result with a resolved timestamp.
///
/// The actual timestamp value is set in the [`MirDataflowDescription`] of the
/// surrounding [`GlobalMirPlan`] when we call `resolve()`.
#[derive(Clone)]
pub struct Resolved;

/// The (final) result after MIR ⇒ LIR lowering and optimizing the resulting
/// `DataflowDescription` with `LIR` plans.
#[derive(Clone)]
pub struct GlobalLirPlan {
    pub df_desc: LirDataflowDescription,
    pub df_meta: DataflowMetainfo,
}

impl OptimizeMaterializedView {
    pub fn new(
        catalog: Arc<Catalog>,
        compute_instance: ComputeInstanceSnapshot,
        exported_sink_id: GlobalId,
        internal_view_id: GlobalId,
        column_names: Vec<ColumnName>,
        non_null_assertions: Vec<usize>,
        debug_name: String,
        config: OptimizerConfig,
    ) -> Self {
        Self {
            typecheck_ctx: empty_context(),
            catalog,
            compute_instance,
            exported_sink_id,
            internal_view_id,
            column_names,
            non_null_assertions,
            debug_name,
            config,
        }
    }
}

impl<'ctx> Optimize<'ctx, HirRelationExpr> for OptimizeMaterializedView {
    type To = LocalMirPlan;

    fn optimize<'s: 'ctx>(&'s mut self, expr: HirRelationExpr) -> Result<Self::To, OptimizerError> {
        // HIR ⇒ MIR lowering and decorrelation
        let config = mz_sql::plan::OptimizerConfig {};
        let expr = expr.optimize_and_lower(&config)?;

        // MIR ⇒ MIR optimization (local)
        let expr = span!(target: "optimizer", Level::TRACE, "local").in_scope(|| {
            let optimizer = TransformOptimizer::logical_optimizer(&self.typecheck_ctx);
            let expr = optimizer.optimize(expr)?.into_inner();

            // Trace the result of this phase.
            trace_plan(&expr);

            Ok::<_, OptimizerError>(expr)
        })?;

        // Return the (sealed) plan at the end of this optimization step.
        Ok(LocalMirPlan { expr })
    }
}

impl LocalMirPlan {
    pub fn expr(&self) -> OptimizedMirRelationExpr {
        OptimizedMirRelationExpr(self.expr.clone())
    }
}

/// This is needed only because the pipeline in the bootstrap code starts from an
/// [`OptimizedMirRelationExpr`] attached to a [`crate::catalog::CatalogItem`].
impl<'ctx> Optimize<'ctx, OptimizedMirRelationExpr> for OptimizeMaterializedView {
    type To = GlobalMirPlan<Unresolved>;

    fn optimize<'s: 'ctx>(
        &'s mut self,
        expr: OptimizedMirRelationExpr,
    ) -> Result<Self::To, OptimizerError> {
        let expr = expr.into_inner();
        self.optimize(LocalMirPlan { expr })
    }
}

impl<'ctx> Optimize<'ctx, LocalMirPlan> for OptimizeMaterializedView {
    type To = GlobalMirPlan<Unresolved>;

    fn optimize<'s: 'ctx>(&'s mut self, plan: LocalMirPlan) -> Result<Self::To, OptimizerError> {
        let LocalMirPlan { expr } = plan;

        let mut rel_typ = expr.typ();
        for &i in self.non_null_assertions.iter() {
            rel_typ.column_types[i].nullable = false;
        }
        let rel_desc = RelationDesc::new(rel_typ, self.column_names.clone());

        let mut df_builder =
            DataflowBuilder::new(self.catalog.state(), self.compute_instance.clone());

        let (df_desc, df_meta) = df_builder.build_materialized_view(
            self.exported_sink_id,
            self.internal_view_id,
            self.debug_name.clone(),
            &OptimizedMirRelationExpr(expr),
            &rel_desc,
            &self.non_null_assertions,
        )?;

        // Return the (sealed) plan at the end of this optimization step.
        Ok(GlobalMirPlan {
            df_desc,
            df_meta,
            ts_info: Unresolved {
                compute_instance_id: self.compute_instance.instance_id(),
            },
        })
    }
}

impl<T: Clone> GlobalMirPlan<T> {
    pub fn df_desc(&self) -> &MirDataflowDescription {
        &self.df_desc
    }

    pub fn df_meta(&self) -> &DataflowMetainfo {
        &self.df_meta
    }
}

impl GlobalMirPlan<Unresolved> {
    /// Produces the [`GlobalMirPlan`] with [`Resolved`] timestamp required for
    /// the next stage.
    pub fn resolve(mut self, as_of: Antichain<Timestamp>) -> GlobalMirPlan<Resolved> {
        // Set the `as_of` timestamp for the dataflow.
        self.df_desc.set_as_of(as_of);

        // If the only outputs of the dataflow are sinks, we might be able to
        // turn off the computation early, if they all have non-trivial
        // `up_to`s.
        //
        // TODO: This should always be the case here so we can demote
        // the outer if to a soft assert.
        if self.df_desc.index_exports.is_empty() {
            self.df_desc.until = Antichain::from_elem(Timestamp::MIN);
            for (_, sink) in &self.df_desc.sink_exports {
                self.df_desc.until.join_assign(&sink.up_to);
            }
        }

        GlobalMirPlan {
            df_desc: self.df_desc,
            df_meta: self.df_meta,
            ts_info: Resolved,
        }
    }

    /// Computes the [`CollectionIdBundle`] of the wrapped dataflow.
    pub fn id_bundle(&self) -> CollectionIdBundle {
        let storage_ids = self.df_desc.source_imports.keys().copied().collect();
        let compute_ids = self.df_desc.index_imports.keys().copied().collect();
        CollectionIdBundle {
            storage_ids,
            compute_ids: btreemap! {self.compute_instance_id() => compute_ids},
        }
    }

    /// Returns the [`ComputeInstanceId`] against which we should resolve the
    /// timestamp for the next stage.
    pub fn compute_instance_id(&self) -> ComputeInstanceId {
        self.ts_info.compute_instance_id
    }
}

impl<'ctx> Optimize<'ctx, GlobalMirPlan<Resolved>> for OptimizeMaterializedView {
    type To = GlobalLirPlan;

    fn optimize<'s: 'ctx>(
        &'s mut self,
        plan: GlobalMirPlan<Resolved>,
    ) -> Result<Self::To, OptimizerError> {
        let GlobalMirPlan {
            mut df_desc,
            df_meta,
            ts_info: _,
        } = plan;

        // Ensure all expressions are normalized before finalizing.
        for build in df_desc.objects_to_build.iter_mut() {
            normalize_lets(&mut build.plan.0)?
        }

        // Finalize the dataflow. This includes:
        // - MIR ⇒ LIR lowering
        // - LIR ⇒ LIR transforms
        let df_desc = Plan::finalize_dataflow(
            df_desc,
            self.config.enable_consolidate_after_union_negate,
            false, // we are not in a monotonic context here
        )
        .map_err(OptimizerError::Internal)?;

        // Return the plan at the end of this `optimize` step.
        Ok(GlobalLirPlan { df_desc, df_meta })
    }
}

impl GlobalLirPlan {
    pub fn unapply(self) -> (LirDataflowDescription, DataflowMetainfo) {
        (self.df_desc, self.df_meta)
    }

    pub fn df_desc(&self) -> &LirDataflowDescription {
        &self.df_desc
    }

    pub fn df_meta(&self) -> &DataflowMetainfo {
        &self.df_meta
    }

    pub fn desc(&self) -> RelationDesc {
        let sink_exports = &self.df_desc.sink_exports;
        let sink = sink_exports.values().next().expect("valid sink");
        sink.from_desc.clone()
    }
}