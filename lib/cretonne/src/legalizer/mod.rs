//! Legalize instructions.
//!
//! A legal instruction is one that can be mapped directly to a machine code instruction for the
//! target ISA. The `legalize_function()` function takes as input any function and transforms it
//! into an equivalent function using only legal instructions.
//!
//! The characteristics of legal instructions depend on the target ISA, so any given instruction
//! can be legal for one ISA and illegal for another.
//!
//! Besides transforming instructions, the legalizer also fills out the `function.encodings` map
//! which provides a legal encoding recipe for every instruction.
//!
//! The legalizer does not deal with register allocation constraints. These constraints are derived
//! from the encoding recipes, and solved later by the register allocator.

use cursor::{Cursor, FuncCursor};
use flowgraph::ControlFlowGraph;
use ir::{self, InstBuilder};
use isa::TargetIsa;
use bitset::BitSet;

mod boundary;
mod globalvar;
mod heap;
mod split;

use self::globalvar::expand_global_addr;
use self::heap::expand_heap_addr;

/// Legalize `func` for `isa`.
///
/// - Transform any instructions that don't have a legal representation in `isa`.
/// - Fill out `func.encodings`.
///
pub fn legalize_function(func: &mut ir::Function, cfg: &mut ControlFlowGraph, isa: &TargetIsa) {
    debug_assert!(cfg.is_valid());

    boundary::legalize_signatures(func, isa);

    func.encodings.resize(func.dfg.num_insts());

    let mut pos = FuncCursor::new(func);

    // Process EBBs in layout order. Some legalization actions may split the current EBB or append
    // new ones to the end. We need to make sure we visit those new EBBs too.
    while let Some(_ebb) = pos.next_ebb() {
        // Keep track of the cursor position before the instruction being processed, so we can
        // double back when replacing instructions.
        let mut prev_pos = pos.position();

        while let Some(inst) = pos.next_inst() {
            let opcode = pos.func.dfg[inst].opcode();

            // Check for ABI boundaries that need to be converted to the legalized signature.
            if opcode.is_call() && boundary::handle_call_abi(inst, pos.func, cfg) {
                // Go back and legalize the inserted argument conversion instructions.
                pos.set_position(prev_pos);
                continue;
            }

            if opcode.is_return() && boundary::handle_return_abi(inst, pos.func, cfg) {
                // Go back and legalize the inserted return value conversion instructions.
                pos.set_position(prev_pos);
                continue;
            }

            if opcode.is_branch() {
                split::simplify_branch_arguments(&mut pos.func.dfg, inst);
            }

            match isa.encode(
                &pos.func.dfg,
                &pos.func.dfg[inst],
                pos.func.dfg.ctrl_typevar(inst),
            ) {
                Ok(encoding) => pos.func.encodings[inst] = encoding,
                Err(action) => {
                    // We should transform the instruction into legal equivalents.
                    let changed = action(inst, pos.func, cfg);
                    // If the current instruction was replaced, we need to double back and revisit
                    // the expanded sequence. This is both to assign encodings and possible to
                    // expand further.
                    // There's a risk of infinite looping here if the legalization patterns are
                    // unsound. Should we attempt to detect that?
                    if changed {
                        pos.set_position(prev_pos);
                        continue;
                    }
                }
            }

            // Remember this position in case we need to double back.
            prev_pos = pos.position();
        }
    }
}

// Include legalization patterns that were generated by `gen_legalizer.py` from the `XForms` in
// `meta/cretonne/legalize.py`.
//
// Concretely, this defines private functions `narrow()`, and `expand()`.
include!(concat!(env!("OUT_DIR"), "/legalizer.rs"));

/// Custom expansion for conditional trap instructions.
/// TODO: Add CFG support to the Python patterns so we won't have to do this.
fn expand_cond_trap(inst: ir::Inst, func: &mut ir::Function, cfg: &mut ControlFlowGraph) {
    // Parse the instruction.
    let trapz;
    let arg = match func.dfg[inst] {
        ir::InstructionData::Unary { opcode, arg } => {
            // We want to branch *over* an unconditional trap.
            trapz = match opcode {
                ir::Opcode::Trapz => true,
                ir::Opcode::Trapnz => false,
                _ => panic!("Expected cond trap: {}", func.dfg.display_inst(inst, None)),
            };
            arg
        }
        _ => panic!("Expected cond trap: {}", func.dfg.display_inst(inst, None)),
    };

    // Split the EBB after `inst`:
    //
    //     trapnz arg
    //
    // Becomes:
    //
    //     brz arg, new_ebb
    //     trap
    //   new_ebb:
    //
    let old_ebb = func.layout.pp_ebb(inst);
    let new_ebb = func.dfg.make_ebb();
    if trapz {
        func.dfg.replace(inst).brnz(arg, new_ebb, &[]);
    } else {
        func.dfg.replace(inst).brz(arg, new_ebb, &[]);
    }

    let mut pos = FuncCursor::new(func).at_inst(inst);
    pos.next_inst();
    pos.ins().trap();
    pos.insert_ebb(new_ebb);

    // Finally update the CFG.
    cfg.recompute_ebb(pos.func, old_ebb);
    cfg.recompute_ebb(pos.func, new_ebb);
}
