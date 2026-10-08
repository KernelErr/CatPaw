pub(crate) mod pass;
pub(crate) mod walker;

use self::{
    pass::{ConstantFolding, DeadCodeElimination, StrengthReduction},
    walker::Walker,
};
use crate::Context;
use bitflags::bitflags;
use boa_ast::{
    Expression, StatementList,
    statement::Statement,
    visitor::{VisitWith, VisitorMut},
};
use std::{fmt, ops::ControlFlow};

bitflags! {
    /// Optimizer options.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct OptimizerOptions: u8 {
        /// Print statistics to `stdout`.
        const STATISTICS = 0b0000_0001;
        /// Apply constant folding optimization.
        const CONSTANT_FOLDING = 0b0000_0010;
        /// Apply strength reduction.
        const STRENGTH_REDUCTION = 0b0000_0100;
        /// Apply dead code elimination.
        const DEAD_CODE_ELIMINATION = 0b0000_1000;
        /// Apply all optimizations.
        const OPTIMIZE_ALL = Self::CONSTANT_FOLDING.bits()
            | Self::STRENGTH_REDUCTION.bits()
            | Self::DEAD_CODE_ELIMINATION.bits();
    }
}

/// The action to be performed after an optimization step.
#[derive(Debug)]
pub(crate) enum PassAction<T> {
    /// Keep the node, do nothing.
    Keep,
    /// The node was modified inplace.
    Modified,
    /// Replace the node.
    Replace(T),
}

/// Contains statistics about the optimizer execution.
#[derive(Debug, Default, Clone, Copy)]
pub struct OptimizerStatistics {
    /// Constant folding run count.
    pub constant_folding_run_count: usize,
    /// Constant folding pass count.
    pub constant_folding_pass_count: usize,
    /// Strength reduction run count.
    pub strength_reduction_run_count: usize,
    /// Strength reduction pass count.
    pub strength_reduction_pass_count: usize,
    /// Dead code elimination count.
    pub dead_code_elimination_count: usize,
}

impl fmt::Display for OptimizerStatistics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Optimizer {{")?;
        writeln!(
            f,
            "    constant folding: {} run(s), {} pass(es) ({} mutating, {} checking)",
            self.constant_folding_run_count,
            self.constant_folding_pass_count,
            self.constant_folding_pass_count
                .saturating_sub(self.constant_folding_run_count),
            self.constant_folding_run_count
        )?;
        writeln!(
            f,
            "    strength reduction: {} run(s), {} pass(es)",
            self.strength_reduction_run_count, self.strength_reduction_pass_count,
        )?;
        writeln!(
            f,
            "    dead code elimination: {} elimination(s)",
            self.dead_code_elimination_count,
        )?;
        writeln!(f, "}}")?;
        Ok(())
    }
}

/// This represents an AST optimizer.
#[derive(Debug)]
pub(crate) struct Optimizer<'context> {
    statistics: OptimizerStatistics,
    context: &'context mut Context,
}

impl<'context> Optimizer<'context> {
    /// Create a optimizer.
    pub(crate) fn new(context: &'context mut Context) -> Self {
        Self {
            statistics: OptimizerStatistics::default(),
            context,
        }
    }

    /// Run the constant folding optimization on an expression.
    fn run_constant_folding_pass(&mut self, expr: &mut Expression) -> bool {
        // CatPaw: one postorder walk reaches the fixed point. A node is folded after
        // its children into a literal, into one of its children (already folded), or
        // (a comma) into a node that folds no further, so another walk would find
        // nothing to do. The published crate walked again to find that out, which
        // for a script wrapped in a function expression meant walking the whole
        // script once more. Debug builds still take that walk, to check.
        self.statistics.constant_folding_run_count += 1;
        self.statistics.constant_folding_pass_count += 1;
        let mut walker = Walker::new(|expr| -> PassAction<Expression> {
            ConstantFolding::fold_expression(expr, self.context)
        });
        // NOTE: postoder traversal is optimal for constant folding,
        // since it evaluates the tree bottom-up.
        walker.walk_expression_postorder(expr);
        let changed = walker.changed();
        if changed {
            // The walk that found nothing more to fold.
            self.statistics.constant_folding_pass_count += 1;
            #[cfg(debug_assertions)]
            {
                let mut check = Walker::new(|expr| -> PassAction<Expression> {
                    ConstantFolding::fold_expression(expr, self.context)
                });
                check.walk_expression_postorder(expr);
                debug_assert!(
                    !check.changed(),
                    "constant folding did not reach its fixed point in one walk"
                );
            }
        }
        changed
    }

    /// Run the strength reduction optimization on an expression.
    fn run_strength_reduction_pass(&mut self, expr: &mut Expression) -> bool {
        // CatPaw: one postorder walk reaches the fixed point, since a reduced node is
        // a multiplication of operands already visited, which reduces no further; see
        // `run_constant_folding_pass`.
        self.statistics.strength_reduction_run_count += 1;
        self.statistics.strength_reduction_pass_count += 1;
        let mut walker = Walker::new(|expr| -> PassAction<Expression> {
            StrengthReduction::reduce_expression(expr)
        });
        walker.walk_expression_postorder(expr);
        let changed = walker.changed();
        if changed {
            // The walk that found nothing more to reduce.
            self.statistics.strength_reduction_pass_count += 1;
            #[cfg(debug_assertions)]
            {
                let mut check = Walker::new(|expr| -> PassAction<Expression> {
                    StrengthReduction::reduce_expression(expr)
                });
                check.walk_expression_postorder(expr);
                debug_assert!(
                    !check.changed(),
                    "strength reduction did not reach its fixed point in one walk"
                );
            }
        }
        changed
    }

    fn run_all(&mut self, expr: &mut Expression) {
        if self
            .context
            .optimizer_options()
            .contains(OptimizerOptions::CONSTANT_FOLDING)
        {
            self.run_constant_folding_pass(expr);
        }
        if self
            .context
            .optimizer_options()
            .contains(OptimizerOptions::STRENGTH_REDUCTION)
        {
            self.run_strength_reduction_pass(expr);
        }
    }

    /// Apply optimizations inplace.
    pub(crate) fn apply(&mut self, statement_list: &mut StatementList) -> OptimizerStatistics {
        let _ = self.visit_statement_list_mut(statement_list);

        #[allow(clippy::print_stdout)]
        if self
            .context
            .optimizer_options()
            .contains(OptimizerOptions::STATISTICS)
        {
            println!("{}", self.statistics);
        }
        self.statistics
    }
}

impl<'ast> VisitorMut<'ast> for Optimizer<'_> {
    type BreakTy = ();

    fn visit_expression_mut(&mut self, node: &'ast mut Expression) -> ControlFlow<Self::BreakTy> {
        self.run_all(node);
        ControlFlow::Continue(())
    }

    fn visit_statement_mut(&mut self, node: &'ast mut Statement) -> ControlFlow<Self::BreakTy> {
        // First, recurse into children so constant folding and
        // strength reduction run on nested expressions.
        node.visit_with_mut(self)?;

        // Then apply dead code elimination if enabled.
        if self
            .context
            .optimizer_options()
            .contains(OptimizerOptions::DEAD_CODE_ELIMINATION)
        {
            match node {
                Statement::If(if_stmt) => {
                    if let PassAction::Replace(replacement) =
                        DeadCodeElimination::try_eliminate_if(if_stmt)
                    {
                        *node = replacement;
                        self.statistics.dead_code_elimination_count += 1;
                    }
                }
                Statement::WhileLoop(while_loop) => {
                    if let PassAction::Replace(replacement) =
                        DeadCodeElimination::try_eliminate_while(while_loop)
                    {
                        *node = replacement;
                        self.statistics.dead_code_elimination_count += 1;
                    }
                }
                Statement::ForLoop(for_loop) => {
                    if let PassAction::Replace(replacement) =
                        DeadCodeElimination::try_eliminate_for(for_loop)
                    {
                        *node = replacement;
                        self.statistics.dead_code_elimination_count += 1;
                    }
                }
                _ => {}
            }
        }

        ControlFlow::Continue(())
    }
}
