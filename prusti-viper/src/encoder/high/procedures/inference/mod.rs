use self::{state::FoldUnfoldState, visitor::Visitor};
use crate::encoder::{errors::SpannedEncodingResult, Encoder};
use prusti_common::config;
use rustc_hir::def_id::DefId;
use vir_crate::{
    common::graphviz::ToGraphviz,
    high::{self as vir_high},
    middle::{self as vir_mid},
};

mod action;
mod ensurer;
mod permission;
mod semantics;
mod state;
mod visitor;

pub(super) fn infer_shape_operations<'v, 'tcx: 'v>(
    encoder: &mut Encoder<'v, 'tcx>,
    proc_def_id: DefId,
    procedure: vir_high::ProcedureDecl,
) -> SpannedEncodingResult<vir_mid::ProcedureDecl> {
    if config::dump_debug_info() {
        let source_filename = encoder.env().source_file_name();
        prusti_common::report::log::report_with_writer(
            "graphviz_method_before_foldunfold",
            format!("{}.{}.dot", source_filename, procedure.name),
            |writer| procedure.to_graphviz(writer).unwrap(),
        );
    }
    let mut visitor = Visitor::new(
        encoder,
        proc_def_id,
        FoldUnfoldState::with_parameters_and_return(
            procedure
                .parameters
                .iter()
                .map(|local| local.variable.clone()),
            procedure.returns.iter().map(|local| local.variable.clone()),
        ),
    );
    let shaped_procedure = visitor.infer_procedure(procedure)?;
    visitor.cancel_crash_graphviz();
    if config::dump_debug_info() {
        let source_filename = encoder.env().source_file_name();
        prusti_common::report::log::report_with_writer(
            "graphviz_method_after_foldunfold",
            format!("{}.{}.dot", source_filename, shaped_procedure.name),
            |writer| shaped_procedure.to_graphviz(writer).unwrap(),
        );
    }
    Ok(shaped_procedure)
}
