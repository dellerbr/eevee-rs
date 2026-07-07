//! The eevee SystemVerilog AST.
//!
//! This is the typed intermediate between the Verible front-end ([`eevee-fe`])
//! and the elaborator ([`eevee-elab`]). It is deliberately a plain data schema
//! with no behavior, mirroring the role of the Python reference's
//! `lang/ast_nodes.py`. It currently covers the synthesizable-RTL subset needed
//! for the P2 vertical slice (modules, variables, `always`, procedural
//! assignments, timing controls, and an expression tree); it is designed to
//! grow toward the full language without reshaping these core nodes.
//!
//! [`eevee-fe`]: ../eevee_fe/index.html
//! [`eevee-elab`]: ../eevee_elab/index.html

#![forbid(unsafe_code)]

use eevee_core::LogicVec;

/// A parsed source unit: the top-level descriptions in one or more files.
#[derive(Debug, Clone, Default)]
pub struct SourceFile {
    pub items: Vec<Item>,
}

/// A top-level description.
#[derive(Debug, Clone)]
pub enum Item {
    Module(Module),
    Package(Package),
    /// A class declared at compilation-unit scope (outside any package/module).
    Class(ClassDecl),
    /// A function/task declared at compilation-unit scope.
    Func(FuncDecl),
    // Interface, ... (later)
}

/// A `package ... endpackage` (its items reuse [`ModuleItem`]: classes,
/// functions, typedefs, params).
#[derive(Debug, Clone)]
pub struct Package {
    pub name: String,
    pub items: Vec<ModuleItem>,
}

/// A `module ... endmodule`.
#[derive(Debug, Clone)]
pub struct Module {
    pub name: String,
    pub ports: Vec<Port>,
    pub items: Vec<ModuleItem>,
}

/// A module port.
#[derive(Debug, Clone)]
pub struct Port {
    pub name: String,
    pub dir: PortDir,
    pub width: u32,
    pub signed: bool,
}

/// Port direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortDir {
    Input,
    Output,
    Inout,
    Ref,
}

/// An item inside a module body.
#[derive(Debug, Clone)]
pub enum ModuleItem {
    Var(VarDecl),
    Always(AlwaysBlock),
    Initial(Stmt),
    Func(FuncDecl),
    Class(ClassDecl),
    /// A named enum member, lowered to a compile-time constant
    /// (`enum {UVM_LOW, ...}` -> `UVM_LOW = 0`, ...).
    EnumConst {
        name: String,
        value: LogicVec,
    },
    /// A named enum type and its members, for `.name()` resolution.
    /// (`typedef enum {UVM_INFO, ...} uvm_severity;`).
    EnumType {
        name: String,
        members: Vec<(String, LogicVec)>,
    },
    /// A package/module-scope `typedef <Type>[#(...)] <alias>;`.
    TypeAlias(TypeAlias),
    // ContinuousAssign, ModuleInstance, Generate, ... (later)
}

/// A (possibly parameterized) type reference, e.g. `int`, `uvm_root`, or
/// `uvm_pool#(string, my_obj)`. Used to carry `#(...)` actual arguments for
/// monomorphization. A value argument (a number/identifier) is stored as a
/// name with no `args`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TypeRef {
    pub name: String,
    pub args: Vec<TypeRef>,
}

impl TypeRef {
    pub fn simple(name: impl Into<String>) -> TypeRef {
        TypeRef {
            name: name.into(),
            args: Vec::new(),
        }
    }
}

/// A variable / net declaration (`logic [W-1:0] name = init;`).
#[derive(Debug, Clone)]
pub struct VarDecl {
    pub name: String,
    pub width: u32,
    pub signed: bool,
    /// `Some(class)` if this is a class handle (a reference, not a bit-vector).
    /// For a collection, this is the *element* class (if the elements are
    /// class handles).
    pub class_name: Option<String>,
    /// Actual `#(...)` type arguments of a parameterized type, e.g. the
    /// `string, my_obj` in `uvm_pool#(string, my_obj) p;`.
    pub type_args: Vec<TypeRef>,
    /// True for a `string`-typed variable.
    pub is_string: bool,
    /// `Some(kind)` if this is a queue / dynamic array / associative array.
    pub coll: Option<CollKind>,
    /// True for a `static` class field (one shared storage, not per-instance).
    pub is_static: bool,
    pub init: Option<Expr>,
}

/// The flavor of an unpacked collection declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollKind {
    /// `[$]` queue, `[]` dynamic array, or `[N]` fixed array (all list-backed).
    Queue,
    /// `[key_type]` associative array.
    Assoc,
}

/// A `class ... endclass` declaration.
#[derive(Debug, Clone)]
pub struct ClassDecl {
    pub name: String,
    pub base: Option<String>,
    pub fields: Vec<VarDecl>,
    pub methods: Vec<FuncDecl>,
    pub constructor: Option<FuncDecl>,
    /// Class-scoped `typedef <Class> <alias>;` aliases (notably the factory
    /// `type_id`). The target carries any `#(...)` args for monomorphization.
    pub type_aliases: Vec<TypeAlias>,
    /// Formal parameters of a parameterized class `class C #(type T=int, ...)`.
    pub params: Vec<ParamDecl>,
    /// Actual `#(...)` arguments on the `extends Base#(args)` clause.
    pub base_args: Vec<TypeRef>,
    /// Class-scoped named constants (`localparam`/`parameter`), gathered into
    /// the global constant table.
    pub consts: Vec<(String, LogicVec)>,
}

/// A class-scoped typedef `typedef <target> <alias>;`.
#[derive(Debug, Clone)]
pub struct TypeAlias {
    pub alias: String,
    pub target: TypeRef,
}

/// A formal parameter of a parameterized class.
#[derive(Debug, Clone)]
pub struct ParamDecl {
    pub name: String,
    /// True for a `type` parameter, false for a value parameter.
    pub is_type: bool,
    /// Default type name (type param) or value text (value param), if declared.
    pub default: Option<String>,
}

/// A `function`/`task` declaration. (Tasks set `is_void` and may contain
/// timing controls; functions return a value of width `ret_width`.)
#[derive(Debug, Clone)]
pub struct FuncDecl {
    pub name: String,
    pub ret_width: u32,
    /// `Some(class)` if the function returns a class handle.
    pub ret_class: Option<String>,
    /// `Some(class)` for an out-of-body (`extern`) definition `Class::method`.
    pub class_scope: Option<String>,
    pub is_void: bool,
    pub is_virtual: bool,
    pub params: Vec<Param>,
    pub body: Stmt,
}

/// A function/task formal parameter.
#[derive(Debug, Clone)]
pub struct Param {
    pub name: String,
    pub dir: PortDir,
    pub width: u32,
    /// `Some(class)` if the parameter is a class handle.
    pub class_name: Option<String>,
    /// Type arguments of a parameterized parameter type (e.g. `#(uvm_callback)`
    /// in `uvm_queue #(uvm_callback) q`). Consumed by mono; empty after that.
    pub type_args: Vec<TypeRef>,
}

/// Which flavor of `always` block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlwaysKind {
    /// `always`
    Plain,
    /// `always_comb`
    Comb,
    /// `always_ff`
    Ff,
    /// `always_latch`
    Latch,
}

/// An `always*` block.
#[derive(Debug, Clone)]
pub struct AlwaysBlock {
    pub kind: AlwaysKind,
    pub body: Stmt,
}

/// A procedural statement.
#[derive(Debug, Clone)]
pub enum Stmt {
    /// `begin ... end`
    Block(Vec<Stmt>),
    /// A local variable declaration inside a procedural block.
    VarDecl(VarDecl),
    /// A timing control prefixing a statement: `@(...) stmt`, `#d stmt`,
    /// `wait(c) stmt`.
    Timed {
        control: TimingControl,
        body: Box<Stmt>,
    },
    /// Blocking assignment `lhs = rhs;`.
    Blocking { lhs: Lvalue, rhs: Expr },
    /// Non-blocking assignment `lhs <= rhs;`.
    Nonblocking { lhs: Lvalue, rhs: Expr },
    /// `if (cond) then [else els]`.
    If {
        cond: Expr,
        then_branch: Box<Stmt>,
        else_branch: Option<Box<Stmt>>,
    },
    /// A system task call statement, e.g. `$display("...", a, b);`.
    SysCall { name: String, args: Vec<Expr> },
    /// An expression evaluated for its side effects (e.g. a void method call).
    Expr(Expr),
    /// `return [expr];`
    Return(Option<Expr>),
    /// Empty statement (`;`).
    Null,
}

/// A timing control.
#[derive(Debug, Clone)]
pub enum TimingControl {
    /// `#expr`
    Delay(Expr),
    /// `@(event_list)`
    Event(Vec<EventExpr>),
    /// `wait(expr)`
    Wait(Expr),
}

/// One entry in an event control list, e.g. `posedge clk`.
#[derive(Debug, Clone)]
pub struct EventExpr {
    pub edge: Edge,
    pub expr: Expr,
}

/// Edge qualifier on an event expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Posedge,
    Negedge,
    /// Bare `@(sig)` — any value change.
    AnyChange,
}

/// An assignment target. (Bit/part selects to be added.)
#[derive(Debug, Clone)]
pub struct Lvalue {
    pub name: String,
    /// `Some(index)` for an element assignment `name[index] = ...`.
    pub index: Option<Expr>,
    /// `Some(class)` for a scoped static-field target `Class::name = ...`.
    pub scope: Option<String>,
}

/// An expression.
#[derive(Debug, Clone)]
pub enum Expr {
    /// A sized/unsized literal, already parsed to a 4-state vector.
    Literal(LogicVec),
    /// A string literal (e.g. a `$display` format string).
    Str(String),
    /// A reference to a variable/net by name.
    Ref(String),
    /// Unary operator.
    Unary { op: UnaryOp, operand: Box<Expr> },
    /// Binary operator.
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    /// A function call returning a value: `name(args...)`.
    Call { name: String, args: Vec<Expr> },
    /// Member access `obj.field`.
    Field { obj: Box<Expr>, field: String },
    /// Method call `obj.method(args...)`.
    MethodCall {
        obj: Box<Expr>,
        method: String,
        args: Vec<Expr>,
    },
    /// Static method call `Class::method(args...)` (scope resolution). The
    /// class name has any `#(...)` parameters stripped (type erasure); the
    /// parameters are kept in `class_args` for monomorphization.
    StaticCall {
        class_name: String,
        class_args: Vec<TypeRef>,
        method: String,
        args: Vec<Expr>,
    },
    /// Static field read `Class::field` (scope resolution, no argument list).
    /// The class name may be a type parameter and needs monomorphization.
    StaticRef { class_name: String, field: String },
    /// Index / element access `base[index]` (queue/array element or assoc key).
    Index { base: Box<Expr>, index: Box<Expr> },
    /// `new(args...)` — allocate an object (class inferred from context).
    New { args: Vec<Expr> },
    /// The `null` class-handle literal.
    Null,
    /// A concatenation `{a, b, c}` (string concatenation for the report path;
    /// bit concatenation is a later refinement).
    Concat(Vec<Expr>),
    /// A system function call in expression position, e.g. `$sformatf(...)`,
    /// `$realtime`, `$cast(...)`, `$time`.
    SysCall { name: String, args: Vec<Expr> },
}

/// Unary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    /// `~`
    BitNot,
    /// `!`
    LogNot,
    /// unary `-`
    Neg,
    /// unary `+`
    Plus,
    /// `&` reduction
    ReduceAnd,
    /// `|` reduction
    ReduceOr,
    /// `^` reduction
    ReduceXor,
}

/// Binary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    And,
    Or,
    Xor,
    Eq,
    Neq,
    Lt,
    Gt,
    Le,
    Ge,
    Shl,
    Shr,
    /// `&&` logical AND.
    LogAnd,
    /// `||` logical OR.
    LogOr,
}
