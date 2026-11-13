// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Value uses and omitted definitions of the instructions selected for emission.
use super::*;
use crate::ir::Node;
use crate::ir_check_elim::CheckElision;
use crate::x64::isel::{checked_integer_array_address, AddrSource, MInst, Ty};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum NodeFlow {
    #[default]
    Graph,
    Selected {
        inputs: Vec<NodeId>,
        folded: bool,
    },
    Absorbed(NodeId),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ValueFlow {
    nodes: Vec<NodeFlow>,
}

pub(crate) struct ValueGroup {
    pub(crate) instructions: Vec<MInst>,
    pub(crate) covered: Vec<NodeId>,
}

type Position = (usize, usize);

fn invalid(why: &'static str) -> Bailout {
    Bailout::new(BailoutReason::UnsupportedShape(why))
}

struct CheckedRead {
    load: NodeId,
    base: NodeId,
}

struct SelectedValues {
    root: NodeId,
    ty: Ty,
    inputs: Vec<NodeId>,
    read: Option<CheckedRead>,
}

fn instruction_values(
    graph: &Graph,
    checks: Option<&CheckElision>,
    instructions: &[MInst],
) -> CompileResult<SelectedValues> {
    let (&instruction, prefix) = instructions
        .split_last()
        .ok_or_else(|| invalid("selected value flow has no instruction"))?;
    instruction
        .probe()
        .map_err(|_| invalid("selected value flow has no instruction encoding"))?;
    let mut read = None;
    let (root, ty, inputs, copy_source) = match instruction {
        MInst::AluRR {
            dst, ty, lhs, rhs, ..
        } => (dst, ty, vec![lhs, rhs], Some(lhs)),
        MInst::AluRI { dst, ty, lhs, .. } => (dst, ty, vec![lhs], Some(lhs)),
        MInst::AluRM { dst, ty, lhs, addr: AddrSource::Expr(addr), load, .. } => {
            let proven = checks.and_then(|checks| checked_integer_array_address(graph, checks, load));
            let (Some(base), Some(index)) = (addr.base, addr.index) else {
                return Err(invalid("selected array load has missing address operands"));
            };
            if proven != Some(addr)
                || !matches!((ty, graph.nodes[load as usize].ty),
                    (Ty::I32, IrType::Int) | (Ty::I64, IrType::Long))
            {
                return Err(invalid("selected array load has no matching bounds proof"));
            }
            read = Some(CheckedRead { load, base });
            (dst, ty, vec![lhs, base, index], Some(lhs))
        }
        MInst::Lea { dst, ty, addr } => (
            dst,
            ty,
            addr.base.into_iter().chain(addr.index).collect(),
            None,
        ),
        _ => {
            return Err(invalid(
                "selected value flow has an unsupported instruction",
            ))
        }
    };
    for &instruction in prefix {
        instruction
            .probe()
            .map_err(|_| invalid("selected value flow has no copy encoding"))?;
        if !matches!(instruction, MInst::Move { dst, ty: Ty::I64, src }
            if dst == root && Some(src) == copy_source)
        {
            return Err(invalid(
                "selected value flow has an unsupported instruction prefix",
            ));
        }
    }
    Ok(SelectedValues { root, ty, inputs, read })
}

fn positions(schedule: &Schedule, n: usize) -> Vec<Option<Position>> {
    let mut positions = vec![None; n];
    for (bi, block) in schedule.blocks.iter().enumerate() {
        for (at, id) in block
            .nodes
            .iter()
            .copied()
            .chain(block.terminator)
            .enumerate()
        {
            if let Some(p) = positions.get_mut(id as usize) {
                *p = Some((bi, at));
            }
        }
    }
    positions
}

fn pins(graph: &Graph, protected: &[NodeId]) -> Vec<bool> {
    let mut pinned = vec![false; graph.nodes.len()];
    for id in graph
        .safepoints
        .iter()
        .flat_map(|sp| sp.locals.iter().chain(&sp.stack).chain(&sp.monitors))
        .chain(protected)
    {
        if let Some(p) = pinned.get_mut(*id as usize) {
            *p = true;
        }
    }
    pinned
}

fn pure_integer(node: &Node) -> bool {
    matches!(node.ty, IrType::Int | IrType::Long)
        && matches!(
            node.op,
            Op::Const(_)
                | Op::Add
                | Op::Sub
                | Op::Mul
                | Op::And
                | Op::Or
                | Op::Xor
                | Op::Shl
                | Op::Shr
                | Op::UShr
                | Op::Neg
        )
}

fn check_root(
    graph: &Graph,
    id: NodeId,
    ty: Ty,
    covered: &[NodeId],
    pos: &[Option<Position>],
) -> CompileResult<Position> {
    let node = graph
        .nodes
        .get(id as usize)
        .ok_or_else(|| invalid("selected value flow has an invalid root"))?;
    if !matches!(
        (node.ty, ty),
        (IrType::Int, Ty::I32) | (IrType::Long, Ty::I64)
    ) {
        return Err(invalid("selected value flow has a mismatched result width"));
    }
    if covered.binary_search(&id).is_err() || covered.windows(2).any(|w| w[0] == w[1]) {
        return Err(invalid("selected value flow has invalid coverage"));
    }
    pos[id as usize].ok_or_else(|| invalid("selected value flow root is unscheduled"))
}

fn check_definition(
    graph: &Graph,
    id: NodeId,
    root: NodeId,
    end: Position,
    positions: &[Option<Position>],
    pinned: &[bool],
    roles: &[NodeFlow],
    checked_load: Option<NodeId>,
) -> CompileResult<()> {
    let i = id as usize;
    let node = graph
        .nodes
        .get(i)
        .ok_or_else(|| invalid("selected value flow covers an invalid definition"))?;
    if !matches!(roles[i], NodeFlow::Graph) || !(pure_integer(node) || checked_load == Some(id)) {
        return Err(invalid(
            "selected value flow covers an owned or unsupported definition",
        ));
    }
    if !positions[i].is_some_and(|(b, at)| b == end.0 && at <= end.1) {
        return Err(invalid(
            "selected value flow crosses a block or definition order",
        ));
    }
    if id != root && pinned[i] {
        return Err(invalid("selected value flow would omit a named value"));
    }
    Ok(())
}

fn check_inputs(
    graph: &Graph,
    inputs: &[NodeId],
    covered: &[NodeId],
    ref_base: Option<NodeId>,
) -> CompileResult<()> {
    let mut frontier = Vec::new();
    for &id in covered {
        for &input in &graph.nodes[id as usize].inputs {
            if covered.binary_search(&input).is_err() {
                frontier.push(input);
            }
        }
    }
    for &id in inputs {
        let node = graph
            .nodes
            .get(id as usize)
            .ok_or_else(|| invalid("selected value flow has an invalid operand"))?;
        let valid_type = matches!(node.ty, IrType::Int | IrType::Long)
            || (node.ty == IrType::Ref && ref_base == Some(id));
        if !valid_type || !frontier.contains(&id) {
            return Err(invalid(
                "selected value flow operand is outside its value boundary",
            ));
        }
    }
    Ok(())
}

impl ValueFlow {
    pub(crate) fn new(
        graph: &Graph,
        schedule: &Schedule,
        groups: Vec<ValueGroup>,
        protected: &[NodeId],
    ) -> CompileResult<Self> {
        Self::new_with_checks(graph, schedule, groups, protected, None)
    }

    pub(crate) fn new_with_checks(
        graph: &Graph,
        schedule: &Schedule,
        groups: Vec<ValueGroup>,
        protected: &[NodeId],
        checks: Option<&CheckElision>,
    ) -> CompileResult<Self> {
        if groups.is_empty() {
            return Ok(Self::default());
        }
        let n = graph.nodes.len();
        let positions = positions(schedule, n);
        let pinned = pins(graph, protected);
        let mut flow = Self {
            nodes: vec![NodeFlow::Graph; n],
        };
        for group in groups {
            let SelectedValues { root, ty, inputs, read } =
                instruction_values(graph, checks, &group.instructions)?;
            let mut covered = group.covered;
            covered.sort_unstable();
            let end = check_root(graph, root, ty, &covered, &positions)?;
            for &id in &covered {
                check_definition(
                    graph, id, root, end, &positions, &pinned, &flow.nodes,
                    read.as_ref().map(|read| read.load),
                )?;
            }
            if let Some(read) = &read {
                let Some((block, at)) = positions[read.load as usize] else {
                    return Err(invalid("selected array read is unscheduled"));
                };
                let order = &schedule.blocks[end.0].nodes;
                if covered.binary_search(&read.load).is_err() || block != end.0 || at >= end.1
                    || !order.get(at + 1..end.1).is_some_and(|between| {
                        between.iter().all(|&mid| {
                            matches!(graph.may_reorder(read.load, mid), crate::ir::Reorder::Allowed(_))
                        })
                    })
                {
                    return Err(invalid("selected array read crosses an observable operation"));
                }
            }
            check_inputs(graph, &inputs, &covered, read.map(|read| read.base))?;
            let folded = covered.len() > 1;
            for id in covered {
                flow.nodes[id as usize] = NodeFlow::Absorbed(root);
            }
            flow.nodes[root as usize] = NodeFlow::Selected { inputs, folded };
        }
        flow.check_remaining_readers(graph, &positions)?;
        Ok(flow)
    }

    fn check_remaining_readers(
        &self,
        graph: &Graph,
        positions: &[Option<Position>],
    ) -> CompileResult<()> {
        for (id, node) in graph.nodes.iter().enumerate() {
            // Value phis read on predecessor edges even when unscheduled.
            if positions[id].is_none() && !matches!(node.op, Op::Phi) {
                continue;
            }
            if matches!(node.op, Op::Phi) && node.ty == IrType::Memory {
                continue;
            }
            for input in self.operands(id as NodeId, node, true) {
                if self.is_absorbed(input) {
                    return Err(invalid(
                        "selected value flow would omit a value another instruction reads",
                    ));
                }
            }
        }
        Ok(())
    }

    pub(crate) fn is_absorbed(&self, id: NodeId) -> bool {
        matches!(self.nodes.get(id as usize), Some(NodeFlow::Absorbed(_)))
    }

    pub(crate) fn requires_selected_emission(&self, id: NodeId) -> bool {
        matches!(
            self.nodes.get(id as usize),
            Some(NodeFlow::Selected { folded: true, .. })
        )
    }

    pub(crate) fn has_absorbed_definitions(&self) -> bool {
        self.nodes
            .iter()
            .any(|role| matches!(role, NodeFlow::Absorbed(_)))
    }

    pub(crate) fn apply_wants(&self, wants: &mut [bool]) {
        for (id, wanted) in wants.iter_mut().enumerate() {
            *wanted &= !self.is_absorbed(id as NodeId);
        }
    }
}

pub(crate) trait ValueSource {
    fn operands<'a>(&'a self, id: NodeId, node: &'a Node, skip_tokens: bool) -> ValueOperands<'a>;
}

pub(crate) struct GraphValues;
impl ValueSource for GraphValues {
    fn operands<'a>(&'a self, _id: NodeId, node: &'a Node, skip_tokens: bool) -> ValueOperands<'a> {
        ValueOperands::graph(node, skip_tokens)
    }
}
impl ValueSource for ValueFlow {
    fn operands<'a>(&'a self, id: NodeId, node: &'a Node, skip_tokens: bool) -> ValueOperands<'a> {
        match self.nodes.get(id as usize) {
            Some(NodeFlow::Selected { inputs, .. }) => ValueOperands::Selected(inputs.iter()),
            Some(NodeFlow::Absorbed(_)) => ValueOperands::Empty,
            _ => ValueOperands::graph(node, skip_tokens),
        }
    }
}

pub(crate) enum ValueOperands<'a> {
    Graph {
        node: &'a Node,
        inputs: std::iter::Enumerate<std::slice::Iter<'a, NodeId>>,
        skip_tokens: bool,
    },
    Selected(std::slice::Iter<'a, NodeId>),
    Empty,
}
impl<'a> ValueOperands<'a> {
    fn graph(node: &'a Node, skip_tokens: bool) -> Self {
        Self::Graph {
            node,
            inputs: node.inputs.iter().enumerate(),
            skip_tokens,
        }
    }
}
impl Iterator for ValueOperands<'_> {
    type Item = NodeId;
    fn next(&mut self) -> Option<NodeId> {
        match self {
            Self::Graph {
                node,
                inputs,
                skip_tokens,
            } => {
                for (slot, &id) in inputs.by_ref() {
                    if !*skip_tokens || !crate::ir::is_memory_token_slot(node, slot) {
                        return Some(id);
                    }
                }
                None
            }
            Self::Selected(inputs) => inputs.next().copied(),
            Self::Empty => None,
        }
    }
}
