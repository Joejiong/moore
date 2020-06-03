// Copyright (c) 2016-2020 Fabian Schuiki

//! Representation of constant values and their operations
//!
//! This module implements a representation for values that may arise within a
//! SystemVerilog source text and provides ways of executing common operations
//! such as addition and multiplication. It also provides the ability to
//! evaluate the constant value of nodes in a context.
//!
//! The operations in this module are intended to panic if invalid combinations
//! of values are used. The compiler's type system should catch and prevent such
//! uses.

use crate::{
    crate_prelude::*,
    hir::HirNode,
    ty::{SbvType, Type, TypeKind},
    ParamEnv, ParamEnvBinding,
};
use bit_vec::BitVec;
use itertools::Itertools;
use num::{BigInt, BigRational, Integer, One, ToPrimitive, Zero};

/// A verilog value.
pub type Value<'t> = &'t ValueData<'t>;

/// The data associated with a value.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ValueData<'t> {
    /// The type of the value.
    pub ty: Type<'t>,
    /// The actual value.
    pub kind: ValueKind<'t>,
}

impl<'t> ValueData<'t> {
    /// Check if the value represents a computation error tombstone.
    pub fn is_error(&self) -> bool {
        self.ty.is_error() || self.kind.is_error()
    }

    /// Check if this value evaluates to true.
    pub fn is_true(&self) -> bool {
        !self.is_false()
    }

    /// Check if this value evaluates to false.
    pub fn is_false(&self) -> bool {
        match self.kind {
            ValueKind::Void => true,
            ValueKind::Int(ref v, ..) => v.is_zero(),
            ValueKind::Time(ref v) => v.is_zero(),
            ValueKind::StructOrArray(_) => false,
            ValueKind::Error => true,
        }
    }

    /// Convert the value to an integer.
    pub fn get_int(&self) -> Option<&BigInt> {
        match self.kind {
            ValueKind::Int(ref v, ..) => Some(v),
            _ => None,
        }
    }
}

/// The different forms a value can assume.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ValueKind<'t> {
    /// The `void` value.
    Void,
    /// An arbitrary precision integer.
    ///
    /// The first field contains the value. The second field indicates the
    /// special bits (x or z), and the third indicates the x bits.
    Int(BigInt, BitVec, BitVec),
    /// An arbitrary precision time interval.
    Time(BigRational),
    /// A struct.
    StructOrArray(Vec<Value<'t>>),
    /// An error occurred during value computation.
    Error,
}

impl<'t> ValueKind<'t> {
    /// Check if the value represents a computation error tombstone.
    pub fn is_error(&self) -> bool {
        match self {
            ValueKind::Error => true,
            _ => false,
        }
    }
}

impl std::fmt::Display for ValueKind<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            ValueKind::Void => write!(f, "void"),
            ValueKind::Int(v, ..) => write!(f, "{}", v),
            ValueKind::Time(v) => write!(f, "{}", v),
            ValueKind::StructOrArray(v) => {
                write!(f, "{{ {} }}", v.iter().map(|v| &v.kind).format(", "))
            }
            ValueKind::Error => write!(f, "<error>"),
        }
    }
}

/// Create a new tombstone value.
pub fn make_error(ty: Type) -> ValueData {
    ValueData {
        ty,
        kind: ValueKind::Error,
    }
}

/// Create a new integer value.
///
/// Panics if `ty` is not an integer type. Truncates the value to `ty`.
pub fn make_int(ty: Type, value: BigInt) -> ValueData {
    let w = ty.width();
    make_int_special(
        ty,
        value,
        BitVec::from_elem(w, false),
        BitVec::from_elem(w, false),
    )
}

/// Create a new integer value with special bits.
///
/// Panics if `ty` is not an integer type. Truncates the value to `ty`.
pub fn make_int_special(
    ty: Type,
    mut value: BigInt,
    special_bits: BitVec,
    x_bits: BitVec,
) -> ValueData {
    match *ty.resolve_name() {
        TypeKind::Int(width, _)
        | TypeKind::BitVector {
            range: ty::Range { size: width, .. },
            ..
        } => {
            value = value % (BigInt::from(1) << width);
        }
        TypeKind::Bit(_) | TypeKind::BitScalar { .. } => {
            value = value % 2;
        }
        _ => panic!("create int value `{}` with non-int type {:?}", value, ty),
    }
    ValueData {
        ty: ty,
        kind: ValueKind::Int(value, special_bits, x_bits),
    }
}

/// Create a new time value.
pub fn make_time(value: BigRational) -> ValueData<'static> {
    ValueData {
        ty: &ty::TIME_TYPE,
        kind: ValueKind::Time(value),
    }
}

/// Create a new struct value.
pub fn make_struct<'t>(ty: Type<'t>, fields: Vec<Value<'t>>) -> ValueData<'t> {
    assert!(ty.is_struct());
    ValueData {
        ty: ty,
        kind: ValueKind::StructOrArray(fields),
    }
}

/// Create a new array value.
pub fn make_array<'t>(ty: Type<'t>, elements: Vec<Value<'t>>) -> ValueData<'t> {
    assert!(ty.is_array());
    ValueData {
        ty: ty,
        kind: ValueKind::StructOrArray(elements),
    }
}

/// Determine the constant value of a node.
pub(crate) fn constant_value_of<'gcx>(
    cx: &impl Context<'gcx>,
    node_id: NodeId,
    env: ParamEnv,
) -> Result<Value<'gcx>> {
    let v = const_node(cx, node_id, env);
    if cx.sess().has_verbosity(Verbosity::CONSTS) {
        let vp = v
            .as_ref()
            .map(|v| format!("{}, {}", v.ty, v.kind))
            .unwrap_or_else(|_| format!("<error>"));
        let span = cx.span(node_id);
        let ext = span.extract();
        let line = span.begin().human_line();
        println!("{}: const({}) = {}", line, ext, vp);
    }
    v
}

fn const_node<'gcx>(
    cx: &impl Context<'gcx>,
    node_id: NodeId,
    env: ParamEnv,
) -> Result<Value<'gcx>> {
    let hir = cx.hir_of(node_id)?;
    match hir {
        HirNode::Expr(expr) => {
            let mir = cx.mir_rvalue(expr.id, env);
            Ok(cx.const_mir_rvalue(mir.into()))
        }
        HirNode::ValueParam(param) => {
            let env_data = cx.param_env_data(env);
            match env_data.find_value(node_id) {
                Some(ParamEnvBinding::Indirect(assigned_id)) => {
                    return cx.constant_value_of(assigned_id.id(), assigned_id.env())
                }
                Some(ParamEnvBinding::Direct(v)) => return Ok(v),
                _ => (),
            }
            if let Some(default) = param.default {
                return cx.constant_value_of(default, env);
            }
            let d = DiagBuilder2::error(format!(
                "{} not assigned and has no default",
                param.desc_full(),
            ));
            let contexts = cx.param_env_contexts(env);
            for &context in &contexts {
                cx.emit(
                    d.clone()
                        .span(cx.span(context))
                        .add_note("Parameter declared here:")
                        .span(param.human_span()),
                );
            }
            if contexts.is_empty() {
                cx.emit(d.span(param.human_span()));
            }
            Err(())
        }
        HirNode::GenvarDecl(decl) => {
            let env_data = cx.param_env_data(env);
            match env_data.find_value(node_id) {
                Some(ParamEnvBinding::Indirect(assigned_id)) => {
                    return cx.constant_value_of(assigned_id.id(), assigned_id.env())
                }
                Some(ParamEnvBinding::Direct(v)) => return Ok(v),
                _ => (),
            }
            if let Some(init) = decl.init {
                return cx.constant_value_of(init, env);
            }
            cx.emit(
                DiagBuilder2::error(format!("{} not initialized", decl.desc_full()))
                    .span(decl.human_span()),
            );
            Err(())
        }
        HirNode::VarDecl(_) => {
            cx.emit(
                DiagBuilder2::error(format!("{} has no constant value", hir.desc_full()))
                    .span(hir.human_span()),
            );
            Err(())
        }
        HirNode::EnumVariant(var) => match var.value {
            Some(v) => cx.constant_value_of(v, env),
            None => Ok(cx.intern_value(make_int(cx.type_of(node_id, env)?, var.index.into()))),
        },
        _ => cx.unimp_msg("constant value computation of", &hir),
    }
}

pub(crate) fn const_mir_rvalue_query<'gcx>(
    cx: &impl Context<'gcx>,
    mir: Ref<'gcx, mir::Rvalue<'gcx>>,
) -> Value<'gcx> {
    const_mir_rvalue(cx, *mir)
}

fn const_mir_rvalue<'gcx>(cx: &impl Context<'gcx>, mir: &'gcx mir::Rvalue<'gcx>) -> Value<'gcx> {
    let v = const_mir_rvalue_inner(cx, mir);
    if cx.sess().has_verbosity(Verbosity::CONSTS) {
        let ext = mir.span.extract();
        let line = mir.span.begin().human_line();
        println!("{}: const_mir({}) = {}, {}", line, ext, v.ty, v.kind);
    }
    v
}

fn const_mir_rvalue_inner<'gcx>(
    cx: &impl Context<'gcx>,
    mir: &'gcx mir::Rvalue<'gcx>,
) -> Value<'gcx> {
    let mir_ty = mir.ty.to_legacy(cx);

    // Propagate MIR tombstones immediately.
    if mir.is_error() {
        return cx.intern_value(make_error(mir_ty));
    }

    match mir.kind {
        // TODO: Casts are just transparent at the moment. That's pretty bad.
        mir::RvalueKind::CastValueDomain { value, .. }
        | mir::RvalueKind::CastVectorToAtom { value, .. }
        | mir::RvalueKind::CastAtomToVector { value, .. }
        | mir::RvalueKind::CastSign(_, value)
        | mir::RvalueKind::Truncate(_, value)
        | mir::RvalueKind::ZeroExtend(_, value)
        | mir::RvalueKind::SignExtend(_, value) => {
            cx.emit(
                DiagBuilder2::warning("cast ignored during constant evaluation")
                    .span(mir.span)
                    .add_note(format!(
                        "Casts `{}` from `{}` to `{}`",
                        value.span.extract(),
                        value.ty,
                        mir_ty
                    ))
                    .span(value.span),
            );
            let v = cx.const_mir_rvalue(value.into());
            // TODO: This is an incredibly ugly hack.
            cx.intern_value(ValueData {
                ty: mir_ty,
                kind: v.kind.clone(),
            })
        }

        mir::RvalueKind::CastToBool(value) => {
            let value = cx.const_mir_rvalue(value.into());
            if value.is_error() {
                return cx.intern_value(make_error(mir_ty));
            }
            cx.intern_value(make_int(mir_ty, (value.is_true() as usize).into()))
        }

        mir::RvalueKind::ConstructArray(ref values) => cx.intern_value(make_array(
            mir_ty,
            (0..values.len())
                .map(|index| cx.const_mir_rvalue(values[&index].into()))
                .collect(),
        )),

        mir::RvalueKind::ConstructStruct(ref values) => cx.intern_value(make_struct(
            mir_ty,
            values
                .iter()
                .map(|&value| cx.const_mir_rvalue(value.into()))
                .collect(),
        )),

        mir::RvalueKind::Const(value) => value,

        mir::RvalueKind::UnaryBitwise { op, arg } => {
            let arg_val = cx.const_mir_rvalue(arg.into());
            if arg_val.is_error() {
                return cx.intern_value(make_error(mir_ty));
            }
            match arg_val.kind {
                ValueKind::Int(ref arg_int, ..) => cx.intern_value(make_int(
                    mir_ty,
                    const_unary_bitwise_int(cx, mir.ty.simple_bit_vector(cx, mir.span), op, arg_int),
                )),
                _ => unreachable!(),
            }
        }

        mir::RvalueKind::BinaryBitwise { op, lhs, rhs } => {
            let lhs_val = cx.const_mir_rvalue(lhs.into());
            let rhs_val = cx.const_mir_rvalue(rhs.into());
            if lhs_val.is_error() || rhs_val.is_error() {
                return cx.intern_value(make_error(mir_ty));
            }
            match (&lhs_val.kind, &rhs_val.kind) {
                (ValueKind::Int(lhs_int, ..), ValueKind::Int(rhs_int, ..)) => {
                    cx.intern_value(make_int(
                        mir_ty,
                        const_binary_bitwise_int(cx, mir.ty.simple_bit_vector(cx, mir.span), op, lhs_int, rhs_int),
                    ))
                }
                _ => unreachable!(),
            }
        }

        mir::RvalueKind::IntUnaryArith { op, arg, .. } => {
            let arg_val = cx.const_mir_rvalue(arg.into());
            if arg_val.is_error() {
                return cx.intern_value(make_error(mir_ty));
            }
            match arg_val.kind {
                ValueKind::Int(ref arg_int, ..) => cx.intern_value(make_int(
                    mir_ty,
                    const_unary_arith_int(cx, mir.ty.simple_bit_vector(cx, mir.span), op, arg_int),
                )),
                _ => unreachable!(),
            }
        }

        mir::RvalueKind::IntBinaryArith { op, lhs, rhs, .. } => {
            let lhs_val = cx.const_mir_rvalue(lhs.into());
            let rhs_val = cx.const_mir_rvalue(rhs.into());
            if lhs_val.is_error() || rhs_val.is_error() {
                return cx.intern_value(make_error(mir_ty));
            }
            match (&lhs_val.kind, &rhs_val.kind) {
                (ValueKind::Int(lhs_int, ..), ValueKind::Int(rhs_int, ..)) => {
                    cx.intern_value(make_int(
                        mir_ty,
                        const_binary_arith_int(cx, mir.ty.simple_bit_vector(cx, mir.span), op, lhs_int, rhs_int),
                    ))
                }
                _ => unreachable!(),
            }
        }

        mir::RvalueKind::IntComp { op, lhs, rhs, .. } => {
            let lhs_val = cx.const_mir_rvalue(lhs.into());
            let rhs_val = cx.const_mir_rvalue(rhs.into());
            if lhs_val.is_error() || rhs_val.is_error() {
                return cx.intern_value(make_error(mir_ty));
            }
            match (&lhs_val.kind, &rhs_val.kind) {
                (ValueKind::Int(lhs_int, ..), ValueKind::Int(rhs_int, ..)) => cx.intern_value(
                    make_int(mir_ty, const_comp_int(cx, mir.ty.simple_bit_vector(cx, mir.span), op, lhs_int, rhs_int)),
                ),
                _ => unreachable!(),
            }
        }

        mir::RvalueKind::Concat(ref values) => {
            let mut result = BigInt::zero();
            for &value in values {
                result <<= value.ty.simple_bit_vector(cx, value.span).size;
                result |= cx
                    .const_mir_rvalue(value.into())
                    .get_int()
                    .expect("concat non-integer");
            }
            cx.intern_value(make_int(mir_ty, result))
        }

        mir::RvalueKind::Repeat(count, value) => {
            let value_const = cx.const_mir_rvalue(value.into());
            if value_const.is_error() {
                return cx.intern_value(make_error(mir_ty));
            }
            let sbvt = value.ty.simple_bit_vector(cx, value.span);
            let mut result = BigInt::zero();
            for _ in 0..count {
                result <<= sbvt.size;
                result |= value_const.get_int().expect("repeat non-integer");
            }
            cx.intern_value(make_int(mir_ty, result))
        }

        mir::RvalueKind::Assignment { .. } | mir::RvalueKind::Var(_) | mir::RvalueKind::Port(_) => {
            cx.emit(DiagBuilder2::error("value is not constant").span(mir.span));
            cx.intern_value(make_error(mir_ty))
        }

        mir::RvalueKind::Member { value, field } => {
            let value_const = cx.const_mir_rvalue(value.into());
            if value_const.is_error() {
                return cx.intern_value(make_error(mir_ty));
            }
            match value_const.kind {
                ValueKind::StructOrArray(ref fields) => fields[field],
                _ => unreachable!("member access on non-struct should be caught in typeck"),
            }
        }

        mir::RvalueKind::Ternary {
            cond,
            true_value,
            false_value,
        } => {
            let cond_val = cx.const_mir_rvalue(cond.into());
            let true_val = cx.const_mir_rvalue(true_value.into());
            let false_val = cx.const_mir_rvalue(false_value.into());
            match cond_val.is_true() {
                true => true_val,
                false => false_val,
            }
        }

        mir::RvalueKind::Shift {
            op,
            arith,
            value,
            amount,
            ..
        } => {
            let value_val = cx.const_mir_rvalue(value.into());
            let amount_val = cx.const_mir_rvalue(amount.into());
            if value_val.is_error() || amount_val.is_error() {
                return cx.intern_value(make_error(mir_ty));
            }
            match (&value_val.kind, &amount_val.kind) {
                (ValueKind::Int(value_int, ..), ValueKind::Int(amount_int, ..)) => {
                    cx.intern_value(make_int(
                        mir_ty,
                        const_shift_int(cx, value.ty.simple_bit_vector(cx, value.span), op, arith, value_int, amount_int),
                    ))
                }
                _ => unreachable!(),
            }
        }

        mir::RvalueKind::Reduction { op, arg } => {
            let arg_val = cx.const_mir_rvalue(arg.into());
            if arg_val.is_error() {
                return cx.intern_value(make_error(mir_ty));
            }
            match arg_val.kind {
                ValueKind::Int(ref arg_int, ..) => cx.intern_value(make_int(
                    mir_ty,
                    const_reduction_int(cx, arg.ty.simple_bit_vector(cx, arg.span), op, arg_int),
                )),
                _ => unreachable!(),
            }
        }

        mir::RvalueKind::Index {
            // value,
            // base,
            // length,
            ..
        } => {
            bug_span!(mir.span, cx, "constant folding of slices not implemented");
        }

        // Propagate tombstones.
        mir::RvalueKind::Error => cx.intern_value(make_error(mir_ty)),
    }
}

fn const_unary_bitwise_int<'gcx>(
    _cx: &impl Context<'gcx>,
    ty: SbvType,
    op: mir::UnaryBitwiseOp,
    arg: &BigInt,
) -> BigInt {
    match op {
        mir::UnaryBitwiseOp::Not => (BigInt::one() << ty.size) - 1 - arg,
    }
}

fn const_binary_bitwise_int<'gcx>(
    _cx: &impl Context<'gcx>,
    _ty: SbvType,
    op: mir::BinaryBitwiseOp,
    lhs: &BigInt,
    rhs: &BigInt,
) -> BigInt {
    match op {
        mir::BinaryBitwiseOp::And => lhs & rhs,
        mir::BinaryBitwiseOp::Or => lhs | rhs,
        mir::BinaryBitwiseOp::Xor => lhs ^ rhs,
    }
}

fn const_unary_arith_int<'gcx>(
    _cx: &impl Context<'gcx>,
    _ty: SbvType,
    op: mir::IntUnaryArithOp,
    arg: &BigInt,
) -> BigInt {
    match op {
        mir::IntUnaryArithOp::Neg => -arg,
    }
}

fn const_binary_arith_int<'gcx>(
    _cx: &impl Context<'gcx>,
    _ty: SbvType,
    op: mir::IntBinaryArithOp,
    lhs: &BigInt,
    rhs: &BigInt,
) -> BigInt {
    match op {
        mir::IntBinaryArithOp::Add => lhs + rhs,
        mir::IntBinaryArithOp::Sub => lhs - rhs,
        mir::IntBinaryArithOp::Mul => lhs * rhs,
        mir::IntBinaryArithOp::Div => lhs / rhs,
        mir::IntBinaryArithOp::Mod => lhs % rhs,
        mir::IntBinaryArithOp::Pow => {
            let mut result = num::one();
            let mut cnt = rhs.clone();
            while !cnt.is_zero() {
                result = result * lhs;
                cnt = cnt - 1;
            }
            result
        }
    }
}

fn const_comp_int<'gcx>(
    _cx: &impl Context<'gcx>,
    _ty: SbvType,
    op: mir::IntCompOp,
    lhs: &BigInt,
    rhs: &BigInt,
) -> BigInt {
    match op {
        mir::IntCompOp::Eq => ((lhs == rhs) as usize).into(),
        mir::IntCompOp::Neq => ((lhs != rhs) as usize).into(),
        mir::IntCompOp::Lt => ((lhs < rhs) as usize).into(),
        mir::IntCompOp::Leq => ((lhs <= rhs) as usize).into(),
        mir::IntCompOp::Gt => ((lhs > rhs) as usize).into(),
        mir::IntCompOp::Geq => ((lhs >= rhs) as usize).into(),
    }
}

fn const_shift_int<'gcx>(
    _cx: &impl Context<'gcx>,
    _ty: SbvType,
    op: mir::ShiftOp,
    _arith: bool,
    value: &BigInt,
    amount: &BigInt,
) -> BigInt {
    match op {
        mir::ShiftOp::Left => match amount.to_isize() {
            Some(sh) if sh < 0 => value >> -sh as usize,
            Some(sh) => value << sh as usize,
            None => num::zero(),
        },
        mir::ShiftOp::Right => match amount.to_isize() {
            Some(sh) if sh < 0 => value << -sh as usize,
            Some(sh) => value >> sh as usize,
            None => num::zero(),
        },
    }
}

fn const_reduction_int<'gcx>(
    _cx: &impl Context<'gcx>,
    ty: SbvType,
    op: mir::BinaryBitwiseOp,
    arg: &BigInt,
) -> BigInt {
    match op {
        mir::BinaryBitwiseOp::And => ((arg == &((BigInt::one() << ty.size) - 1)) as usize).into(),
        mir::BinaryBitwiseOp::Or => ((!arg.is_zero()) as usize).into(),
        mir::BinaryBitwiseOp::Xor => (arg
            .to_bytes_le()
            .1
            .into_iter()
            .map(|v| v.count_ones())
            .sum::<u32>()
            .is_odd() as usize)
            .into(),
    }
}

/// Check if a node has a constant value.
pub(crate) fn is_constant<'gcx>(cx: &impl Context<'gcx>, node_id: NodeId) -> Result<bool> {
    let hir = cx.hir_of(node_id)?;
    Ok(match hir {
        HirNode::ValueParam(_) => true,
        HirNode::GenvarDecl(_) => true,
        HirNode::EnumVariant(_) => true,
        _ => false,
    })
}

/// Determine the default value of a type.
pub(crate) fn type_default_value<'gcx>(cx: &impl Context<'gcx>, ty: Type<'gcx>) -> Value<'gcx> {
    match *ty {
        TypeKind::Error => cx.intern_value(ValueData {
            ty: &ty::ERROR_TYPE,
            kind: ValueKind::Error,
        }),
        TypeKind::Void => cx.intern_value(ValueData {
            ty: &ty::VOID_TYPE,
            kind: ValueKind::Void,
        }),
        TypeKind::Time => cx.intern_value(make_time(Zero::zero())),
        TypeKind::Bit(..)
        | TypeKind::Int(..)
        | TypeKind::BitVector { .. }
        | TypeKind::BitScalar { .. } => cx.intern_value(make_int(ty, Zero::zero())),
        TypeKind::Named(_, _, ty) => type_default_value(cx, ty),
        TypeKind::Struct(id) => {
            let def = cx.struct_def(id.id()).unwrap();
            let fields = def
                .fields
                .iter()
                .map(|field| type_default_value(cx, cx.map_to_type(field.ty, id.env()).unwrap()))
                .collect();
            cx.intern_value(make_struct(ty, fields))
        }
        TypeKind::PackedArray(length, elem_ty) => cx.intern_value(make_array(
            ty.clone(),
            std::iter::repeat(cx.type_default_value(elem_ty.clone()))
                .take(length)
                .collect(),
        )),
    }
}
