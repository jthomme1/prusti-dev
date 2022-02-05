use super::Visitor;
use vir_crate::{
    common::graphviz::{Graph, NodeBuilder},
    middle::{self as vir_mid},
};

impl<'p, 'v, 'tcx> Visitor<'p, 'v, 'tcx> {
    fn render_crash_state(&self) -> Graph {
        let mut graph = Graph::with_columns(&["statement"]);
        for (label, block) in &self.basic_blocks {
            let mut node_builder = self.create_node_builder(label, &mut graph);
            self.render_block(label, block, &mut node_builder);
            node_builder.build();
            self.render_successor(label, &block.successor, &mut graph);
        }
        graph
    }
    fn is_crash_label(&self, label: &vir_mid::BasicBlockId) -> bool {
        if let Some(crash_label) = self.current_label.as_ref() {
            crash_label == label
        } else {
            false
        }
    }
    fn create_node_builder<'a>(
        &self,
        label: &vir_mid::BasicBlockId,
        graph: &'a mut Graph,
    ) -> NodeBuilder<'a> {
        if self.is_crash_label(label) {
            graph.create_node_with_custom_style(label.to_string(), "bgcolor=\"red\"".to_string())
        } else if self.successfully_processed_blocks.contains(label) {
            graph.create_node_with_custom_style(label.to_string(), "bgcolor=\"green\"".to_string())
        } else {
            graph.create_node(label.to_string())
        }
    }
    fn render_block(
        &self,
        label: &vir_mid::BasicBlockId,
        block: &vir_mid::BasicBlock,
        node_builder: &mut NodeBuilder,
    ) {
        for statement in &block.statements {
            let statement_string = match statement {
                vir_mid::Statement::Comment(statement) => {
                    format!("<font color=\"orange\">{}</font>", statement)
                }
                _ => statement.to_string(),
            };
            node_builder.add_row_sequence(vec![statement_string]);
        }
        if self.is_crash_label(label) {
            for statement in &self.current_statements {
                let statement_string = format!("<font color=\"red\">{}</font>", statement);
                node_builder.add_row_sequence(vec![statement_string]);
            }
        }
    }
    fn render_successor(
        &self,
        label: &vir_mid::BasicBlockId,
        successor: &vir_mid::Successor,
        graph: &mut Graph,
    ) {
        match successor {
            vir_mid::Successor::Return => {
                graph.add_exit_edge(label.to_string(), "return".to_string())
            }
            vir_mid::Successor::Goto(target) => {
                graph.add_regular_edge(label.to_string(), target.to_string())
            }
            vir_mid::Successor::GotoSwitch(targets) => {
                for (_, target) in targets {
                    graph.add_regular_edge(label.to_string(), target.to_string());
                }
            }
        }
    }
}

impl<'p, 'v, 'tcx> Drop for Visitor<'p, 'v, 'tcx> {
    fn drop(&mut self) {
        if self.graphviz_on_crash {
            let graph = self.render_crash_state();
            let source_filename = self.encoder.env().source_file_name();
            let procedure_name = self.procedure_name.take().unwrap();
            // TODO: Include all relevant information:
            // 1. Fold-unfold state.
            // 2. Mark which nodes were successfully visited.
            // 3. Mark which edges were successfully visited.
            // 4. Mark where the crash happened.
            prusti_common::report::log::report_with_writer(
                "graphviz_method_crashing_foldunfold",
                format!("{}.{}.dot", source_filename, procedure_name),
                |writer| graph.write(writer).unwrap(),
            );
        }
    }
}
