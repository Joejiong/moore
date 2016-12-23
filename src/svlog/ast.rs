// Copyright (c) 2016 Fabian Schuiki

use source::Span;
use name::Name;


pub struct ModDecl {
	pub span: Span,
	pub lifetime: Lifetime, // default static
	pub name: Name,
	pub name_span: Span,
	pub ports: Vec<Port>,
}

pub struct IntfDecl {
	pub span: Span,
	pub lifetime: Lifetime, // default static
	pub name: Name,
	pub name_span: Span,
	pub ports: Vec<Port>,
}



/// Lifetime specifier for variables, tasks, and functions. Defaults to static.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lifetime {
	Static,
	Automatic,
}



#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Type {
	pub span: Span,
	pub data: TypeData,
	pub sign: TypeSign,
	pub dims: Vec<TypeDim>,
}

pub use self::TypeData::*;
pub use self::PortKind::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeData {
	ImplicitType,
	NamedType(Name),

	// Integer Vector Types
	BitType,
	LogicType,
	RegType,

	// Integer Atom Types
	ByteType,
	ShortIntType,
	IntType,
	LongIntType,
	TimeType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeSign {
	None,
	Signed,
	Unsigned,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeDim {
	Expr,
	Range,
	Queue,
	Unsized,
	Associative,
}



#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Port {
	pub span: Span,
	pub name: Name,
	pub name_span: Span,
	// If kind, type, direction all omitted, inherit from previous port.
	pub kind: PortKind, // input,inout => net, output w. impl. type => net, output w. expl. type => var, ref => var
	pub ty: Type, // default logic
	pub dir: PortDir, // inherit or default inout if first
	pub dims: Vec<TypeDim>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortKind {
	NetPort,
	VarPort,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortDir {
	Input,
	Output,
	Inout,
	Ref,
}



#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamPort {
	pub span: Span,
	pub name: Name,
	pub name_span: Span,
	pub ty: Type,
	pub dims: Vec<TypeDim>,
	pub init: (),
}
