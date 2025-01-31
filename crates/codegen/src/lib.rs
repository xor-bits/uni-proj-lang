use std::{fmt, iter, marker::PhantomData};

use inkwell::{
    basic_block::BasicBlock,
    builder::Builder,
    context::Context,
    execution_engine::ExecutionEngine,
    module::Module,
    types::FunctionType,
    values::{
        AnyValue, BasicMetadataValueEnum, BasicValue, BasicValueEnum, FunctionValue, PointerValue,
    },
    AddressSpace, OptimizationLevel,
};
use parser::ast::{Ast, BinaryOp, Root};
use typeck::{BlockId, FuncId, Function, Statement, TmpId, Type, VarId};

use self::types::{AsLlvm, AsLlvmConst};
pub use self::types::{AsType, FnAsLlvm, Str};

//

mod types;

//

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Clone)]
pub enum Error {
    NoMainFn,
    InvalidMainFn,
    StaticRedefined(String),
    VariableNotFound(String),
    Type(typeck::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::NoMainFn => write!(f, "no main function"),
            Error::InvalidMainFn => write!(f, "invalid main function signature"),
            Error::StaticRedefined(name) => write!(f, "static `{name}` already defined"),
            Error::VariableNotFound(name) => write!(f, "variable `{name}` not found"),
            Error::Type(e) => write!(f, "{e}"),
        }
    }
}

impl From<typeck::Error> for Error {
    fn from(value: typeck::Error) -> Self {
        Self::Type(value)
    }
}

//

fn context() -> &'static Context {
    thread_local! {
        static CTX: &'static Context = Box::leak(Box::new(Context::create()));
    }

    CTX.with(|c| *c)
}

fn to_prototype(gen: &ModuleGen, code: &typeck::Module, func: &Function) -> FunctionType<'static> {
    let param_types: Box<[_]> = func
        .params
        .iter()
        .map(|ty| code.get_type(*ty).as_llvm_meta(gen).unwrap())
        .collect();

    code.get_type(func.returns)
        .as_llvm_fn(gen, &param_types, false)
}

//

pub trait IndexOf {
    fn index(self) -> usize;
}

impl IndexOf for VarId {
    fn index(self) -> usize {
        self.0
    }
}

impl IndexOf for TmpId {
    fn index(self) -> usize {
        self.0
    }
}

impl IndexOf for FuncId {
    fn index(self) -> usize {
        self.0
    }
}

impl IndexOf for BlockId {
    fn index(self) -> usize {
        self.0
    }
}

//

pub struct IdMap<K, V> {
    vals: Vec<Option<V>>,
    _p: PhantomData<K>,
}

impl<K, V: Copy> IdMap<K, V> {
    pub const fn new() -> Self {
        Self {
            vals: Vec::new(),
            _p: PhantomData,
        }
    }

    fn reserve(&mut self, new_len: usize) {
        if self.vals.len() < new_len {
            self.vals.resize(new_len, None);
        }
    }

    fn clear(&mut self) {
        self.vals.clear();
    }
}

impl<K, V: Copy> Default for IdMap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Copy + IndexOf, V> IdMap<K, V> {
    #[track_caller]
    fn get(&self, k: K) -> &V {
        // println!("get {}", k.index());
        self.vals[k.index()].as_ref().unwrap()
    }

    #[track_caller]
    fn set(&mut self, k: K, v: V) {
        // println!("set {}", k.index());
        self.vals[k.index()] = Some(v);
    }
}

//

pub struct CodeGen {
    ctx: Option<&'static Context>,
}

impl CodeGen {
    pub const fn new() -> Self {
        Self { ctx: None }
    }

    pub fn module(&mut self) -> ModuleGen {
        let ctx = *self.ctx.get_or_insert_with(context);

        let module = ctx.create_module("<run>");
        let builder = ctx.create_builder();
        let alloca_builder = ctx.create_builder();

        let engine = module
            .create_jit_execution_engine(OptimizationLevel::Aggressive)
            .unwrap();

        ModuleGen {
            ctx,
            module,
            builder,
            alloca_builder,

            engine,

            types: typeck::Module::new(),
            functions: IdMap::new(),
        }
    }
}

impl Default for CodeGen {
    fn default() -> Self {
        Self::new()
    }
}

//

pub struct ModuleGen {
    ctx: &'static Context,
    module: Module<'static>,
    builder: Builder<'static>,
    alloca_builder: Builder<'static>,

    engine: ExecutionEngine<'static>,

    types: typeck::Module,
    functions: IdMap<FuncId, FunctionValue<'static>>,
}

impl ModuleGen {
    pub fn add(&mut self, ast: &Ast<Root>) -> Result<FuncId> {
        // let main = code.get_function(main);

        let main = self.types.process(ast)?;
        self.types.dump();

        // self.functions.clear();
        self.functions.reserve(self.types.functions().len());

        // generate all function prototypes

        for (i, func) in self.types.functions().iter().enumerate() {
            if func.is_extern {
                continue;
            }

            let proto = to_prototype(self, &self.types, func);
            let func = self
                .module
                .add_function("fixme-keep-function-name", proto, None);

            self.functions.set(FuncId(i), func);
        }

        // compile all functions

        #[derive(Clone, Copy)]
        enum FuncOr<T> {
            T(T),
            FunctionValue(FunctionValue<'static>),
        }

        impl<T> FuncOr<T> {
            fn as_t(&self) -> Option<&T> {
                if let Self::T(v) = self {
                    Some(v)
                } else {
                    None
                }
            }

            fn as_function_value(&self) -> Option<&FunctionValue<'static>> {
                if let Self::FunctionValue(v) = self {
                    Some(v)
                } else {
                    None
                }
            }
        }

        let mut tmp_map: IdMap<TmpId, FuncOr<BasicValueEnum>> = IdMap::new();
        let mut var_map: IdMap<VarId, FuncOr<PointerValue>> = IdMap::new();
        let mut block_map: IdMap<BlockId, BasicBlock> = IdMap::new();

        for (i, func) in self.types.functions().iter().enumerate() {
            if func.is_extern {
                continue;
            }

            let func_val = *self.functions.get(FuncId(i));

            let entry = self.ctx.append_basic_block(func_val, "allocas");
            self.alloca_builder.position_at_end(entry);

            tmp_map.clear();
            var_map.clear();
            block_map.clear();
            tmp_map.reserve(func.temporaries.len());
            var_map.reserve(func.variables.len());
            block_map.reserve(func.blocks.len());

            for (block_id, _) in func.blocks() {
                let block = self.ctx.append_basic_block(
                    func_val,
                    if block_id.0 == 0 {
                        "entry"
                    } else {
                        "fixme-keep-labels"
                    },
                );
                block_map.set(block_id, block);
            }

            for (block_id, code_block) in func.blocks() {
                self.builder.position_at_end(*block_map.get(block_id));

                for stmt in code_block.stmts.iter() {
                    match stmt {
                        Statement::Let { dst, src } => match *tmp_map.get(*src) {
                            FuncOr::T(val) => {
                                let ty = val.get_type();
                                let ptr = self
                                    .alloca_builder
                                    .build_alloca(ty, "fixme-keep-variable-name")
                                    .unwrap();

                                var_map.set(*dst, FuncOr::T(ptr));
                                self.builder.build_store(ptr, val).unwrap();
                            }
                            FuncOr::FunctionValue(val) => {
                                var_map.set(*dst, FuncOr::FunctionValue(val));
                            }
                        },
                        Statement::Store { dst, src } => {
                            let ptr = *var_map
                                .get(*dst)
                                .as_t()
                                .expect("cannot mutate a function value");
                            let val = *tmp_map
                                .get(*src)
                                .as_t()
                                .expect("cannot mutate a function value");
                            self.builder.build_store(ptr, val).unwrap();
                        }
                        Statement::Load { dst, src } => match var_map.get(*src) {
                            FuncOr::T(ptr) => {
                                let val = self
                                    .builder
                                    .build_load(
                                        self.types.get_type(func.var(*src)).as_llvm(self).unwrap(),
                                        *ptr,
                                        "fixme-keep-variable-name",
                                    )
                                    .unwrap();

                                tmp_map.set(*dst, FuncOr::T(val));
                            }
                            FuncOr::FunctionValue(f) => {
                                tmp_map.set(*dst, FuncOr::FunctionValue(*f));
                            }
                        },
                        Statement::Extern { dst, src, .. } => {
                            let func = *self.functions.get(*src);
                            tmp_map.set(*dst, FuncOr::FunctionValue(func));
                        }
                        Statement::Func { dst, src } => {
                            let func = *self.functions.get(*src);
                            tmp_map.set(*dst, FuncOr::FunctionValue(func));
                        }
                        Statement::Const { dst, src } => {
                            tmp_map.set(*dst, FuncOr::T(src.as_llvm_const(self).unwrap()));
                        }
                        Statement::BinExpr { dst, lhs, op, rhs } => {
                            let lhs_val = *tmp_map
                                .get(*lhs)
                                .as_t()
                                .expect("cannot operate on a function value");
                            let rhs_val = *tmp_map
                                .get(*rhs)
                                .as_t()
                                .expect("cannot operate on a function value");
                            let ty = self.types.get_type(func.tmp(*lhs));

                            let res = match (ty, op) {
                                (Type::I32, BinaryOp::Add) => self
                                    .builder
                                    .build_int_add(
                                        lhs_val.into_int_value(),
                                        rhs_val.into_int_value(),
                                        "builtin-i32-add",
                                    )
                                    .unwrap()
                                    .as_basic_value_enum(),
                                (Type::I32, BinaryOp::Sub) => self
                                    .builder
                                    .build_int_sub(
                                        lhs_val.into_int_value(),
                                        rhs_val.into_int_value(),
                                        "builtin-i32-sub",
                                    )
                                    .unwrap()
                                    .as_basic_value_enum(),
                                (Type::I32, BinaryOp::Mul) => self
                                    .builder
                                    .build_int_mul(
                                        lhs_val.into_int_value(),
                                        rhs_val.into_int_value(),
                                        "builtin-i32-mul",
                                    )
                                    .unwrap()
                                    .as_basic_value_enum(),
                                (Type::I32, BinaryOp::Div) => self
                                    .builder
                                    .build_int_signed_div(
                                        lhs_val.into_int_value(),
                                        rhs_val.into_int_value(),
                                        "builtin-i32-div",
                                    )
                                    .unwrap()
                                    .as_basic_value_enum(),
                                (Type::I32, BinaryOp::Rem) => self
                                    .builder
                                    .build_int_signed_rem(
                                        lhs_val.into_int_value(),
                                        rhs_val.into_int_value(),
                                        "builtin-i32-rem",
                                    )
                                    .unwrap()
                                    .as_basic_value_enum(),
                                (Type::I32, BinaryOp::Ge) => self
                                    .builder
                                    .build_int_compare(
                                        inkwell::IntPredicate::SGE,
                                        lhs_val.into_int_value(),
                                        rhs_val.into_int_value(),
                                        "builtin-i32-ge",
                                    )
                                    .unwrap()
                                    .as_basic_value_enum(),
                                (Type::I32, BinaryOp::Le) => self
                                    .builder
                                    .build_int_compare(
                                        inkwell::IntPredicate::SLE,
                                        lhs_val.into_int_value(),
                                        rhs_val.into_int_value(),
                                        "builtin-i32-le",
                                    )
                                    .unwrap()
                                    .as_basic_value_enum(),
                                (Type::I32, BinaryOp::Gt) => self
                                    .builder
                                    .build_int_compare(
                                        inkwell::IntPredicate::SLT,
                                        lhs_val.into_int_value(),
                                        rhs_val.into_int_value(),
                                        "builtin-i32-gt",
                                    )
                                    .unwrap()
                                    .as_basic_value_enum(),
                                (Type::I32, BinaryOp::Lt) => self
                                    .builder
                                    .build_int_compare(
                                        inkwell::IntPredicate::SLT,
                                        lhs_val.into_int_value(),
                                        rhs_val.into_int_value(),
                                        "builtin-i32-lt",
                                    )
                                    .unwrap()
                                    .as_basic_value_enum(),
                                (Type::I32, BinaryOp::Eq) => self
                                    .builder
                                    .build_int_compare(
                                        inkwell::IntPredicate::EQ,
                                        lhs_val.into_int_value(),
                                        rhs_val.into_int_value(),
                                        "builtin-i32-eq",
                                    )
                                    .unwrap()
                                    .as_basic_value_enum(),
                                (Type::I32, BinaryOp::Neq) => self
                                    .builder
                                    .build_int_compare(
                                        inkwell::IntPredicate::NE,
                                        lhs_val.into_int_value(),
                                        rhs_val.into_int_value(),
                                        "builtin-i32-eq",
                                    )
                                    .unwrap()
                                    .as_basic_value_enum(),
                                (Type::Bool, BinaryOp::And) => self
                                    .builder
                                    .build_and(
                                        lhs_val.into_int_value(),
                                        rhs_val.into_int_value(),
                                        "builtin-bool-and",
                                    )
                                    .unwrap()
                                    .as_basic_value_enum(),
                                (Type::Bool, BinaryOp::Or) => self
                                    .builder
                                    .build_or(
                                        lhs_val.into_int_value(),
                                        rhs_val.into_int_value(),
                                        "builtin-bool-or",
                                    )
                                    .unwrap()
                                    .as_basic_value_enum(),
                                _ => {
                                    todo!("invalid operation: {ty:?} {}", op.as_str());
                                }
                            };

                            tmp_map.set(*dst, FuncOr::T(res));
                        }
                        Statement::Call { dst, func, args } => {
                            let tmp = tmp_map
                                .get(*func)
                                .as_function_value()
                                .expect("cannot call a non function");

                            let args: Box<[_]> = args
                                .iter()
                                .map(|arg| {
                                    tmp_map
                                        .get(*arg)
                                        .as_t()
                                        .expect("cannot use functions as arguments")
                                        .as_basic_value_enum()
                                        .into()
                                })
                                .collect();

                            let val = self
                                .builder
                                .build_direct_call(*tmp, &args, "fixme-keep-function-names")
                                .unwrap();

                            let val = match val.try_as_basic_value().left() {
                                Some(val) => val,
                                None => context().struct_type(&[], false).const_zero().into(),
                            };

                            tmp_map.set(*dst, FuncOr::T(val));
                        }
                        Statement::Return { .. } => todo!(),
                        Statement::ReturnVoid => {
                            self.builder.build_return(None).unwrap();
                        }
                        Statement::UnconditionalJump { id } => {
                            let block = *block_map.get(*id);
                            self.builder.build_unconditional_branch(block).unwrap();
                        }
                        Statement::ConditionalJump {
                            bool,
                            then_block,
                            else_block,
                        } => {
                            let bool = tmp_map
                                .get(*bool)
                                .as_t()
                                .expect("cannot use functions as values")
                                .into_int_value();
                            let then_block = *block_map.get(*then_block);
                            let else_block = *block_map.get(*else_block);
                            self.builder
                                .build_conditional_branch(bool, then_block, else_block)
                                .unwrap();
                        }
                    } // match
                } // for
            } // for

            self.alloca_builder
                .build_unconditional_branch(*block_map.get(BlockId(0)))
                .unwrap();
            // self.builder.build_return(None).unwrap();

            if !func_val.verify(true) {
                eprintln!("LLVM IR:\n");
                self.module.print_to_stderr();
                panic!("invalid fn");
            }
        }

        Ok(main)
    }

    pub fn add_extern<F: FnAsLlvm>(&mut self, name: &str, f: F) -> Result<()> {
        let ret = f.return_type();
        let params = f.params();

        let param_types: Vec<_> = params.iter().filter_map(|a| a.as_llvm_meta(self)).collect();
        let wrapper_ty = ret.as_llvm_fn(self, &param_types, false);
        let wrapper_ptr = self.module.add_function(name, wrapper_ty, None);

        let func_id = self.types.add_extern(name, ret, params);
        self.functions.set(func_id, wrapper_ptr);

        let entry = self.ctx.append_basic_block(wrapper_ptr, "entry");
        self.builder.position_at_end(entry);

        let ty_usize = self
            .ctx
            .ptr_sized_int_type(self.engine.get_target_data(), None);
        let ty_ptr = ty_usize.ptr_type(AddressSpace::default());
        let fn_ptr = self
            .builder
            .build_int_to_ptr(
                ty_usize.const_int(f.as_extern_c_fn_ptr() as _, false),
                ty_ptr,
                "wrapped-fn-ptr",
            )
            .unwrap();

        let args: Vec<BasicMetadataValueEnum> = wrapper_ptr
            .get_param_iter()
            .map(|p| p.as_any_value_enum().try_into().unwrap())
            .collect();

        let val = self
            .builder
            .build_indirect_call(wrapper_ty, fn_ptr, &args, "call-fn-ptr")
            .unwrap();

        match val.try_as_basic_value().left() {
            Some(BasicValueEnum::ArrayValue(v)) => todo!("{v}"),
            Some(BasicValueEnum::IntValue(v)) => {
                self.builder.build_return(Some(&v)).unwrap();
            }
            Some(BasicValueEnum::FloatValue(v)) => todo!("{v}"),
            Some(BasicValueEnum::PointerValue(v)) => todo!("{v}"),
            Some(BasicValueEnum::StructValue(v)) => todo!("{v}"),
            Some(BasicValueEnum::VectorValue(v)) => todo!("{v}"),
            None => {
                self.builder.build_return(None).unwrap();
            }
        };

        if !wrapper_ptr.verify(true) {
            eprintln!("LLVM IR:\n");
            self.module.print_to_stderr();
            panic!("invalid fn");
        }

        Ok(())
    }

    /// # Safety
    /// the fn_ptr should be `extern "C"` signature should match `ret` and `args`
    pub unsafe fn add_extern_userdata(
        &mut self,
        name: &str,
        fn_ptr: usize,
        userdata: usize,
        ret: Type,
        params: &[Type],
    ) -> Result<()> {
        let ty_usize = self
            .ctx
            .ptr_sized_int_type(self.engine.get_target_data(), None);

        let param_types: Vec<_> = iter::once(ty_usize.into())
            .chain(params.iter().filter_map(|a| a.as_llvm_meta(self)))
            .collect();
        let wrapped_ty = ret.as_llvm_fn(self, &param_types, false);

        let param_types: Vec<_> = params.iter().filter_map(|a| a.as_llvm_meta(self)).collect();
        let wrapper_ty = ret.as_llvm_fn(self, &param_types, false);
        let wrapper_ptr = self.module.add_function(name, wrapper_ty, None);

        let func_id = self.types.add_extern(name, ret, params);
        self.functions.reserve(func_id.0 + 1);
        self.functions.set(func_id, wrapper_ptr);

        let entry = self.ctx.append_basic_block(wrapper_ptr, "entry");
        self.builder.position_at_end(entry);

        let ty_ptr = ty_usize.ptr_type(AddressSpace::default());
        let fn_ptr = self
            .builder
            .build_int_to_ptr(
                ty_usize.const_int(fn_ptr as _, false),
                ty_ptr,
                "wrapped-fn-ptr",
            )
            .unwrap();

        let userdata = ty_usize.const_int(userdata as _, false).into();
        let params = wrapper_ptr
            .get_param_iter()
            .map(|p| p.as_any_value_enum().try_into().unwrap());

        let args: Vec<BasicMetadataValueEnum> = iter::once(userdata).chain(params).collect();

        let val = self
            .builder
            .build_indirect_call(wrapped_ty, fn_ptr, &args, "call-fn-ptr")
            .unwrap();

        match val.try_as_basic_value().left() {
            Some(BasicValueEnum::ArrayValue(v)) => todo!("{v}"),
            Some(BasicValueEnum::IntValue(v)) => {
                self.builder.build_return(Some(&v)).unwrap();
            }
            Some(BasicValueEnum::FloatValue(v)) => todo!("{v}"),
            Some(BasicValueEnum::PointerValue(v)) => todo!("{v}"),
            Some(BasicValueEnum::StructValue(v)) => todo!("{v}"),
            Some(BasicValueEnum::VectorValue(v)) => todo!("{v}"),
            None => {
                self.builder.build_return(None).unwrap();
            }
        };

        if !wrapper_ptr.verify(true) {
            eprintln!("LLVM IR:\n");
            self.module.print_to_stderr();
            panic!("invalid fn");
        }

        Ok(())
    }

    pub fn run(&mut self, main: FuncId) {
        // eprintln!("LLVM IR:\n");
        // self.module.print_to_stderr();
        // panic!();

        self.module.verify().unwrap();

        // FIXME: validate the main function signature
        unsafe {
            self.engine.run_function(*self.functions.get(main), &[]);
        }
    }
}

/*use std::{
    collections::{hash_map::Entry, HashMap},
    fmt, iter,
    rc::Rc,
};

use inkwell as llvm;
use llvm::{
    basic_block::BasicBlock,
    builder::Builder,
    context::Context,
    execution_engine::ExecutionEngine,
    module::Module,
    values::{
        AnyValue, BasicMetadataValueEnum, BasicValueEnum, FunctionValue, IntValue, PointerValue,
        StructValue,
    },
    AddressSpace, IntPredicate, OptimizationLevel,
};
use parser::{
    ast::{self, BinaryOp},
    TypeId,
};

//

pub use types::{AsLlvm, FnAsLlvm, Str};

//

mod types;

//

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Clone)]
pub enum Error {
    NoMainFn,
    InvalidMainFn,
    StaticRedefined(String),
    VariableNotFound(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::NoMainFn => write!(f, "no main function"),
            Error::InvalidMainFn => write!(f, "invalid main function signature"),
            Error::StaticRedefined(name) => write!(f, "static `{name}` already defined"),
            Error::VariableNotFound(name) => write!(f, "variable `{name}` not found"),
        }
    }
}

//

pub struct CodeGen {
    ctx: Option<&'static Context>,
}

impl CodeGen {
    pub const fn new() -> Self {
        Self { ctx: None }
    }

    pub fn module(&mut self) -> ModuleGen {
        let ctx = *self.ctx.get_or_insert_with(context);

        let module = ctx.create_module("<run>");
        let builder = ctx.create_builder();
        let alloca_builder = ctx.create_builder();

        let engine = module
            .create_jit_execution_engine(OptimizationLevel::Aggressive)
            .unwrap();

        ModuleGen {
            ctx,
            module,
            builder,
            alloca_builder,

            engine,
            types: typeck::Context::new(),
            fns: HashMap::new(),
            statics: HashMap::new(),
            namespace: "<run>".to_string(),
            locals: Vec::new(),
        }
    }
}

//

pub struct ModuleGen {
    ctx: &'static Context,
    module: Module<'static>,
    builder: Builder<'static>,
    alloca_builder: Builder<'static>,

    engine: ExecutionEngine<'static>,

    types: typeck::Context,

    fns: HashMap<FuncId, FuncValue>,

    statics: HashMap<Rc<str>, Value>,
    namespace: String,
    locals: Vec<(
        HashMap<Rc<str>, Option<PointerValue<'static>>>,
        FunctionValue<'static>,
        BasicBlock<'static>,
    )>,
}

impl ModuleGen {
    pub fn add(&mut self, mut ast: ast::Ast<ast::Root>) -> Result<()> {
        ast.type_check(&mut self.types);
        ast.emit_ir(self)?;
        Ok(())
    }

    pub fn add_extern<F: FnAsLlvm>(&mut self, name: &str, f: F) -> Result<()> {
        let ret = f.return_type();
        let args = f.args().to_vec();

        let param_types: Vec<_> = args.iter().filter_map(|a| a.as_llvm_meta(self)).collect();
        let wrapper_ty = ret.as_llvm_fn(self, &param_types, false);
        let wrapper_ptr = self.module.add_function(name, wrapper_ty, None);

        self.types.add_extern(name, Func { ret, args });

        match self.statics.entry(name.into()) {
            Entry::Occupied(_) => return Err(Error::StaticRedefined(name.into())),
            Entry::Vacant(entry) => entry.insert(Value::Func(FuncValue {
                // data: None,
                fn_ptr: wrapper_ptr,
            })),
        };

        let entry = self.ctx.append_basic_block(wrapper_ptr, "entry");
        self.builder.position_at_end(entry);

        let ty_usize = self
            .ctx
            .ptr_sized_int_type(self.engine.get_target_data(), None);
        let ty_ptr = ty_usize.ptr_type(AddressSpace::default());
        let fn_ptr = self
            .builder
            .build_int_to_ptr(
                ty_usize.const_int(f.as_extern_c_fn_ptr() as _, false),
                ty_ptr,
                "wrapped-fn-ptr",
            )
            .unwrap();

        let args: Vec<BasicMetadataValueEnum> = wrapper_ptr
            .get_param_iter()
            .map(|p| p.as_any_value_enum().try_into().unwrap())
            .collect();

        let val = self
            .builder
            .build_indirect_call(wrapper_ty, fn_ptr, &args, "call-fn-ptr")
            .unwrap();

        let val = match val.try_as_basic_value().left() {
            Some(BasicValueEnum::ArrayValue(v)) => todo!("{v}"),
            Some(BasicValueEnum::IntValue(v)) => Ok(Value::I32(v)),
            Some(BasicValueEnum::FloatValue(v)) => todo!("{v}"),
            Some(BasicValueEnum::PointerValue(v)) => todo!("{v}"),
            Some(BasicValueEnum::StructValue(v)) => todo!("{v}"),
            Some(BasicValueEnum::VectorValue(v)) => todo!("{v}"),
            None => Ok(Value::Void),
        }?;

        self.build_return(val);

        if !wrapper_ptr.verify(true) {
            eprintln!("LLVM IR:\n");
            self.module.print_to_stderr();
            panic!("invalid fn");
        }

        Ok(())
    }

    /// # Safety
    /// the fn_ptr should be `extern "C"` signature should match `ret` and `args`
    pub unsafe fn add_extern_userdata(
        &mut self,
        name: &str,
        fn_ptr: usize,
        userdata: usize,
        ret: TypeId,
        args: Vec<TypeId>,
    ) -> Result<()> {
        let ty_usize = self
            .ctx
            .ptr_sized_int_type(self.engine.get_target_data(), None);

        let param_types: Vec<_> = iter::once(ty_usize.into())
            .chain(args.iter().filter_map(|a| a.as_llvm_meta(self)))
            .collect();
        let wrapped_ty = ret.as_llvm_fn(self, &param_types, false);

        let param_types: Vec<_> = args.iter().filter_map(|a| a.as_llvm_meta(self)).collect();
        let wrapper_ty = ret.as_llvm_fn(self, &param_types, false);
        let wrapper_ptr = self.module.add_function(name, wrapper_ty, None);

        self.types.add_extern(name, Func { ret, args });

        match self.statics.entry(name.into()) {
            Entry::Occupied(_) => return Err(Error::StaticRedefined(name.into())),
            Entry::Vacant(entry) => entry.insert(Value::Func(FuncValue {
                // data: None,
                fn_ptr: wrapper_ptr,
            })),
        };

        let entry = self.ctx.append_basic_block(wrapper_ptr, "entry");
        self.builder.position_at_end(entry);

        let ty_ptr = ty_usize.ptr_type(AddressSpace::default());
        let fn_ptr = self
            .builder
            .build_int_to_ptr(
                ty_usize.const_int(fn_ptr as _, false),
                ty_ptr,
                "wrapped-fn-ptr",
            )
            .unwrap();

        let userdata = ty_usize.const_int(userdata as _, false).into();
        let params = wrapper_ptr
            .get_param_iter()
            .map(|p| p.as_any_value_enum().try_into().unwrap());

        let args: Vec<BasicMetadataValueEnum> = iter::once(userdata).chain(params).collect();

        let val = self
            .builder
            .build_indirect_call(wrapped_ty, fn_ptr, &args, "call-fn-ptr")
            .unwrap();

        let val = match val.try_as_basic_value().left() {
            Some(BasicValueEnum::ArrayValue(v)) => todo!("{v}"),
            Some(BasicValueEnum::IntValue(v)) => Ok(Value::I32(v)),
            Some(BasicValueEnum::FloatValue(v)) => todo!("{v}"),
            Some(BasicValueEnum::PointerValue(v)) => todo!("{v}"),
            Some(BasicValueEnum::StructValue(v)) => todo!("{v}"),
            Some(BasicValueEnum::VectorValue(v)) => todo!("{v}"),
            None => Ok(Value::Void),
        }?;

        self.build_return(val);

        if !wrapper_ptr.verify(true) {
            eprintln!("LLVM IR:\n");
            self.module.print_to_stderr();
            panic!("invalid fn");
        }

        Ok(())
    }

    fn build_return(&self, val: Value) {
        match val {
            Value::Func(_) => todo!(),
            Value::Bool(v) => _ = self.builder.build_return(Some(&v)).unwrap(),
            Value::I32(v) => _ = self.builder.build_return(Some(&v)).unwrap(),
            Value::Str(v) => _ = self.builder.build_return(Some(&v)).unwrap(),
            Value::Never => {}
            Value::Void => _ = self.builder.build_return(None).unwrap(),
        };
    }

    pub fn run(&mut self) -> Result<i32> {
        // eprintln!("LLVM IR:\n");
        // self.module.print_to_stderr();
        // panic!();

        // FIXME: validate the main function signature
        let main_fn = unsafe {
            self.engine
                .get_function::<unsafe extern "C" fn() -> i32>("<run>::main")
        }
        .expect("main not found");

        Ok(unsafe { main_fn.call() })
    }
}

//

#[derive(Debug, Clone)]
enum Value {
    Func(FuncValue),
    // IntLit(i128), // int literal that could be any int type (i8, i32, usize, ...)
    Bool(IntValue<'static>),
    I32(IntValue<'static>),
    Str(StructValue<'static>),
    Never,
    Void,
}

impl Value {
    /// Returns `true` if the value is [`Func`].
    ///
    /// [`Func`]: Value::Func
    #[must_use]
    fn is_func(&self) -> bool {
        matches!(self, Self::Func(..))
    }

    /// Returns `true` if the value is [`Bool`].
    ///
    /// [`Bool`]: Value::Bool
    #[must_use]
    fn is_bool(&self) -> bool {
        matches!(self, Self::Bool(..))
    }

    /// Returns `true` if the value is [`I32`].
    ///
    /// [`I32`]: Value::I32
    #[must_use]
    fn is_i32(&self) -> bool {
        matches!(self, Self::I32(..))
    }

    /// Returns `true` if the value is [`Str`].
    ///
    /// [`Str`]: Value::Str
    #[must_use]
    fn is_str(&self) -> bool {
        matches!(self, Self::Str(..))
    }

    /// Returns `true` if the value is [`Never`].
    ///
    /// [`Never`]: Value::Never
    #[must_use]
    fn is_never(&self) -> bool {
        matches!(self, Self::Never)
    }

    /// Returns `true` if the value is [`Void`].
    ///
    /// [`Void`]: Value::Void
    #[must_use]
    fn is_void(&self) -> bool {
        matches!(self, Self::Void)
    }
}

#[derive(Debug, Clone, Copy)]
struct FuncValue {
    // data: Option<Box<Value>>,
    fn_ptr: FunctionValue<'static>,
}

//

trait EmitIr {
    type Val;

    #[allow(unused)]
    fn emit_ir_partial(&self, gen: &mut ModuleGen) -> Result<()> {
        Ok(())
    }

    fn emit_ir(&self, gen: &mut ModuleGen) -> Result<Self::Val>;
}

impl<T: EmitIr> EmitIr for ast::Ast<T> {
    type Val = T::Val;

    fn emit_ir(&self, gen: &mut ModuleGen) -> Result<Self::Val> {
        self.inner.emit_ir(gen)
    }
}

impl EmitIr for ast::Root {
    type Val = ();

    fn emit_ir(&self, gen: &mut ModuleGen) -> Result<Self::Val> {
        // collect all functions and compile the prototypes before the bodies
        for item in self.inner.iter() {
            match item {
                ast::RootItem::Init(init) => init.emit_ir_partial(gen)?,
                _ => todo!(), // ast::RootItem::Test(test) => test.emit_ir(gen)?,
            }
        }

        // then compile all the bodies
        for item in self.inner.iter() {
            match item {
                ast::RootItem::Init(init) => init.emit_ir(gen)?,
                _ => todo!(), // ast::RootItem::Test(test) => test.emit_ir(gen)?,
            }
        }

        Ok(())
    }
}

impl EmitIr for ast::RootInit {
    type Val = ();

    fn emit_ir_partial(&self, gen: &mut ModuleGen) -> Result<()> {
        assert_eq!(self.targets.iter().len(), self.exprs.iter().len());
        for (target, expr) in self.targets.iter().zip(self.exprs.iter()) {
            let var_name = target.path.ident.value.as_str();
            gen.namespace.push_str("::");
            gen.namespace.push_str(var_name);

            let v = match &expr.expr {
                ast::AnyExpr::Func(f) => Value::Func(f.proto.emit_ir(gen)?),
                _ => expr.eval()?.emit_ir(gen)?,
            };

            match gen.statics.entry(var_name.into()) {
                Entry::Occupied(_) => return Err(Error::StaticRedefined(var_name.into())),
                Entry::Vacant(entry) => entry.insert(v),
            };

            gen.namespace
                .truncate(gen.namespace.len() - 2 - var_name.len());
        }
        Ok(())
    }

    fn emit_ir(&self, gen: &mut ModuleGen) -> Result<Self::Val> {
        assert_eq!(self.targets.iter().len(), self.exprs.iter().len());
        for (_, expr) in self.targets.iter().zip(self.exprs.iter()) {
            if let ast::AnyExpr::Func(f) = &expr.expr {
                f.emit_ir(gen)?
            };
        }
        Ok(())
    }
}

impl EmitIr for ast::Proto {
    type Val = FuncValue;

    fn emit_ir(&self, gen: &mut ModuleGen) -> Result<Self::Val> {
        let func_id = gen
            .types
            .get_type(self.ty)
            .as_func()
            .expect("a function should be a function");
        let func = gen.types.get_func(func_id);

        let param_types: Vec<_> = func
            .args
            .iter()
            .filter_map(|id| id.as_llvm_meta(gen))
            .collect();

        let proto = func.ret.as_llvm_fn(gen, &param_types, false);
        let fn_val = gen.module.add_function(&gen.namespace, proto, None);

        let val = FuncValue {
            // data: None,
            fn_ptr: fn_val,
        };

        if gen.fns.insert(func_id, val).is_some() {
            panic!("function re-defined");
        }

        Ok(val)
    }
}

impl EmitIr for ast::Func {
    type Val = ();

    fn emit_ir(&self, gen: &mut ModuleGen) -> Result<Self::Val> {
        // TODO: generic functions are generated lazily

        let func_id = gen
            .types
            .get_type(self.proto.ty)
            .as_func()
            .expect("a function should be a function");
        let fn_val = gen.fns.get(&func_id).unwrap().fn_ptr;

        let entry = gen.ctx.append_basic_block(fn_val, "entry");
        gen.alloca_builder.position_at_end(entry);

        let code = gen.ctx.append_basic_block(fn_val, "code");
        gen.builder.position_at_end(code);

        let mut locals = HashMap::new();
        for (param, arg) in fn_val.get_param_iter().zip(self.proto.args()) {
            if let BasicValueEnum::IntValue(int) = param {
                let addr = gen
                    .builder
                    .build_alloca(gen.ctx.i32_type(), &arg.id.value)
                    .unwrap();
                gen.builder.build_store(addr, int).unwrap();
                locals.insert(arg.id.value.as_str().into(), Some(addr));
            } else {
                todo!()
            }
        }

        gen.locals.push((locals, fn_val, entry));
        let val = self.block.emit_ir(gen)?;
        gen.locals.pop();

        gen.alloca_builder.position_at_end(entry);
        gen.alloca_builder.build_unconditional_branch(code).unwrap();

        gen.build_return(val);

        if !fn_val.verify(true) {
            eprintln!("LLVM IR:\n");
            gen.module.print_to_stderr();
            panic!("invalid fn");
        }

        Ok(())
    }
}

impl EmitIr for ast::Block {
    type Val = Value;

    fn emit_ir(&self, gen: &mut ModuleGen) -> Result<Self::Val> {
        let mut last = None;
        for stmt in self.stmts.iter() {
            match stmt.emit_ir(gen)? {
                Value::Never => return Ok(Value::Never),
                v => last = Some(v),
            };
        }

        if !self.auto_return {
            return Ok(Value::Void);
        }

        Ok(last.unwrap_or(Value::Void))
    }
}

impl EmitIr for ast::Stmt {
    type Val = Value;

    fn emit_ir(&self, gen: &mut ModuleGen) -> Result<Self::Val> {
        match self {
            ast::Stmt::Init(init) => {
                init.emit_ir(gen)?;
                Ok(Value::Void)
            }
            ast::Stmt::Set(set) => {
                set.emit_ir(gen)?;
                Ok(Value::Void)
            }
            ast::Stmt::Cond(v) => {
                v.emit_ir(gen)?;
                Ok(Value::Void)
            }
            ast::Stmt::Loop(l) => {
                l.emit_ir(gen)?;
                Ok(Value::Void)
            }
            ast::Stmt::Expr(expr) => expr.expr.emit_ir(gen),
            ast::Stmt::Return(expr) => {
                if let Some(expr) = expr.expr.as_ref() {
                    let val = expr.emit_ir(gen)?;
                    gen.build_return(val);
                } else {
                    gen.build_return(Value::Void);
                }
                Ok(Value::Never)
            }
        }
    }
}

impl EmitIr for ast::Cond {
    type Val = ();

    fn emit_ir(&self, gen: &mut ModuleGen) -> Result<Self::Val> {
        /*
            if b0 {
                a0
            } else if b1 {
                a1
            } else {
                a2
            }

            =>

            if b0 {
                a0
            } else {
                if b1 {
                    a1
                } else {
                    a2
                }
            }

            =>

            br b0, if-b0, else-b0

            if-b0:
                a0
                jmp done
            else-b0:
                br b1, if-b1, else-b1
            if-b1:
                a1
                jmp done
            else-b1:
                a2
                jmp done

            done:
        */

        let current_func = gen.locals.last().unwrap().1;
        let done_block = gen.ctx.append_basic_block(current_func, "branch-done");

        let mut last_val = None;
        for test in iter::once(&self.if_first).chain(self.else_ifs.iter().map(|s| &s.inner)) {
            let then_block = gen.ctx.append_basic_block(current_func, "branch-then");
            let else_block = gen.ctx.append_basic_block(current_func, "branch-else");

            let Value::Bool(bool) = test.check.emit_ir(gen)? else {
                unreachable!("{:?}", test.check)
            };
            gen.builder
                .build_conditional_branch(bool, then_block, else_block)
                .unwrap();

            gen.builder.position_at_end(then_block);

            last_val = Some(test.block.emit_ir(gen)?);
            if !last_val.as_ref().unwrap().is_never() {
                gen.builder.build_unconditional_branch(done_block).unwrap();
            }
            gen.builder.position_at_end(else_block);
        }

        if let Some(else_last) = self.else_last.as_ref() {
            last_val = Some(else_last.block.emit_ir(gen)?);
        }

        if !last_val.as_ref().unwrap().is_never() {
            gen.builder.build_unconditional_branch(done_block).unwrap();
        }
        gen.builder.position_at_end(done_block);

        Ok(())
    }
}

impl EmitIr for ast::Loop {
    type Val = ();

    fn emit_ir(&self, gen: &mut ModuleGen) -> Result<Self::Val> {
        let current_func = gen.locals.last().unwrap().1;
        let loop_block = gen.ctx.append_basic_block(current_func, "loop");

        gen.builder.build_unconditional_branch(loop_block).unwrap();
        gen.builder.position_at_end(loop_block);
        self.block.emit_ir(gen).unwrap();
        gen.builder.build_unconditional_branch(loop_block).unwrap();

        let after = gen.ctx.append_basic_block(current_func, "loop-after");
        gen.builder.position_at_end(after);

        Ok(())
    }
}

impl EmitIr for ast::Init {
    type Val = ();

    fn emit_ir(&self, gen: &mut ModuleGen) -> Result<Self::Val> {
        assert_eq!(self.targets.iter().len(), self.exprs.iter().len());
        for (target, expr) in self.targets.iter().zip(self.exprs.iter()) {
            let var_name = target.path.ident.value.as_str();
            gen.namespace.push_str("::");
            gen.namespace.push_str(var_name);

            let v = expr.emit_ir(gen)?;

            let addr = expr.ty.as_llvm(gen).map(|v_ty| {
                let (_, _, entry) = gen.locals.last().unwrap();
                gen.alloca_builder.position_at_end(*entry);

                let addr = gen.alloca_builder.build_alloca(v_ty, var_name).unwrap();
                match v {
                    Value::Func(_) => todo!(),
                    Value::Bool(v) => _ = gen.builder.build_store(addr, v).unwrap(),
                    Value::I32(v) => _ = gen.builder.build_store(addr, v).unwrap(),
                    Value::Str(v) => _ = gen.builder.build_store(addr, v).unwrap(),
                    Value::Never => todo!(),
                    Value::Void => todo!(),
                }

                addr
            });

            // shadow the old var
            _ = gen
                .locals
                .last_mut()
                .unwrap()
                .0
                .insert(var_name.into(), addr);

            gen.namespace
                .truncate(gen.namespace.len() - 2 - var_name.len());
        }
        Ok(())
    }
}

impl EmitIr for ast::Set {
    type Val = ();

    fn emit_ir(&self, gen: &mut ModuleGen) -> Result<Self::Val> {
        assert_eq!(self.targets.iter().len(), self.exprs.iter().len());
        for (target, expr) in self.targets.iter().zip(self.exprs.iter()) {
            let var_name = target.path.ident.value.as_str();
            gen.namespace.push_str("::");
            gen.namespace.push_str(var_name);

            if let Some(addr) = *gen.locals.last_mut().unwrap().0.get(var_name).unwrap() {
                match expr.emit_ir(gen)? {
                    Value::Func(_) => todo!(),
                    Value::Bool(v) => _ = gen.builder.build_store(addr, v).unwrap(),
                    Value::I32(v) => _ = gen.builder.build_store(addr, v).unwrap(),
                    Value::Str(v) => _ = gen.builder.build_store(addr, v).unwrap(),
                    Value::Never => todo!(),
                    Value::Void => todo!(),
                }
            }

            gen.namespace
                .truncate(gen.namespace.len() - 2 - var_name.len());
        }
        Ok(())
    }
}

impl EmitIr for ast::Expr {
    type Val = Value;

    fn emit_ir(&self, gen: &mut ModuleGen) -> Result<Self::Val> {
        match &self.expr {
            ast::AnyExpr::Block(_) => todo!(),
            ast::AnyExpr::LitInt(v) => {
                let val = gen.ctx.i32_type().const_int(v.value as u64, false);
                Ok(Value::I32(val))
            }
            ast::AnyExpr::LitStr(v) => {
                // let str_arr = gen.ctx.const_string(v.value.as_bytes(), false);
                // let ptr = gen
                //     .builder
                //     .build_alloca(str_arr.get_type(), "const-str")
                //     .unwrap();

                // let ptr = gen
                //     .builder
                //     .build_struct_gep(str_arr.get_type(), ptr, 0, "alloca-ptr")
                //     .unwrap();

                let str_arr = gen.ctx.const_string(v.value.as_bytes(), false);
                let x = gen
                    .module
                    .add_global(str_arr.get_type(), None, "global-str");
                x.set_initializer(&str_arr);

                // let x = gen
                //     .builder
                //     .build_global_string_ptr(&v.value, "tmp")
                //     .unwrap();

                // gen.builder.build_store(ptr, str_arr).unwrap();

                let str_type = TypeId::Str.as_llvm(gen).unwrap().into_struct_type();
                Ok(Value::Str(
                    str_type.const_named_struct(&[
                        gen.ctx
                            .ptr_sized_int_type(gen.engine.get_target_data(), None)
                            .const_int(v.value.len() as _, false)
                            .into(),
                        x.as_pointer_value().into(),
                    ]),
                ))
            }
            ast::AnyExpr::Load(var) => {
                if let Some((locals, _, _)) = gen.locals.last() {
                    if let Some(addr) = locals.get(var.value.as_str()).cloned() {
                        let val = addr.map(|addr| (self.ty.as_llvm(gen).unwrap(), addr)).map(
                            |(pointee_ty, addr)| {
                                gen.builder.build_load(pointee_ty, addr, "tmp").unwrap()
                            },
                        );

                        // TODO: match gen.types.get_type(self.ty) {
                        //     Type::Bool => Ok(Value::Bool(val.unwrap().into_int_value())),
                        //     Type::I32 => Ok(Value::I32(val.unwrap().into_int_value())),
                        //     Type::Str => Ok(Value::Str(val.unwrap().into_struct_value())),
                        //     Type::Void => todo!(),
                        //     Type::Never => todo!(),
                        //     Type::Func(_) => todo!(),
                        // }
                        return match self.ty {
                            TypeId::Bool => Ok(Value::Bool(val.unwrap().into_int_value())),
                            TypeId::I32 => Ok(Value::I32(val.unwrap().into_int_value())),
                            TypeId::Str => Ok(Value::Str(val.unwrap().into_struct_value())),
                            TypeId::Void => Ok(Value::Void),
                            TypeId::Never => Ok(Value::Never),
                            TypeId::Unknown => unreachable!(),
                            TypeId::Other(_) => todo!(),
                        };
                    }
                }

                if let Some(val) = gen.statics.get(var.value.as_str()).cloned() {
                    return Ok(val);
                }

                Err(Error::VariableNotFound(var.value.clone()))
            }
            ast::AnyExpr::Func(f) => {
                let func = f.proto.emit_ir(gen)?;
                f.emit_ir(gen)?;

                Ok(Value::Func(func))
            }
            ast::AnyExpr::Binary { op, sides } => {
                let (lhs, rhs) = sides.emit_ir(gen)?;
                match (*op, lhs, rhs) {
                    (BinaryOp::Mul, Value::I32(lhs), Value::I32(rhs)) => Ok(Value::I32(
                        gen.builder.build_int_mul(lhs, rhs, "tmp").unwrap(),
                    )),
                    (BinaryOp::Div, Value::I32(lhs), Value::I32(rhs)) => Ok(Value::I32(
                        gen.builder.build_int_signed_div(lhs, rhs, "tmp").unwrap(),
                    )),
                    (BinaryOp::Rem, Value::I32(lhs), Value::I32(rhs)) => Ok(Value::I32(
                        gen.builder.build_int_signed_rem(lhs, rhs, "tmp").unwrap(),
                    )),

                    (BinaryOp::Add, Value::I32(lhs), Value::I32(rhs)) => Ok(Value::I32(
                        gen.builder.build_int_add(lhs, rhs, "tmp").unwrap(),
                    )),
                    (BinaryOp::Sub, Value::I32(lhs), Value::I32(rhs)) => Ok(Value::I32(
                        gen.builder.build_int_sub(lhs, rhs, "tmp").unwrap(),
                    )),

                    (BinaryOp::Lt, Value::I32(lhs), Value::I32(rhs)) => Ok(Value::Bool(
                        gen.builder
                            .build_int_compare(IntPredicate::SLT, lhs, rhs, "tmp")
                            .unwrap(),
                    )),
                    (BinaryOp::Le, Value::I32(lhs), Value::I32(rhs)) => Ok(Value::Bool(
                        gen.builder
                            .build_int_compare(IntPredicate::SLE, lhs, rhs, "tmp")
                            .unwrap(),
                    )),
                    (BinaryOp::Gt, Value::I32(lhs), Value::I32(rhs)) => Ok(Value::Bool(
                        gen.builder
                            .build_int_compare(IntPredicate::SGT, lhs, rhs, "tmp")
                            .unwrap(),
                    )),
                    (BinaryOp::Ge, Value::I32(lhs), Value::I32(rhs)) => Ok(Value::Bool(
                        gen.builder
                            .build_int_compare(IntPredicate::SGE, lhs, rhs, "tmp")
                            .unwrap(),
                    )),

                    (BinaryOp::Eq, Value::I32(lhs), Value::I32(rhs)) => Ok(Value::Bool(
                        gen.builder
                            .build_int_compare(IntPredicate::EQ, lhs, rhs, "tmp")
                            .unwrap(),
                    )),
                    (BinaryOp::Neq, Value::I32(lhs), Value::I32(rhs)) => Ok(Value::Bool(
                        gen.builder
                            .build_int_compare(IntPredicate::NE, lhs, rhs, "tmp")
                            .unwrap(),
                    )),

                    (BinaryOp::And, Value::Bool(lhs), Value::Bool(rhs)) => {
                        Ok(Value::Bool(gen.builder.build_and(lhs, rhs, "tmp").unwrap()))
                    }
                    (BinaryOp::Or, Value::Bool(lhs), Value::Bool(rhs)) => {
                        Ok(Value::Bool(gen.builder.build_or(lhs, rhs, "tmp").unwrap()))
                    }

                    (op, lhs, rhs) => todo!("{lhs:?} {op} {rhs:?}"),
                }
            }
            ast::AnyExpr::Call(call) => {
                let func = call.func.emit_ir(gen)?;
                let func = match func {
                    Value::Func(f) => f,
                    Value::Bool(_) => todo!(),
                    Value::I32(_) => todo!(),
                    Value::Str(_) => todo!(),
                    Value::Never => todo!(),
                    Value::Void => todo!(),
                };

                let mut args = Vec::new();
                for arg in call.args.iter().flat_map(|a| a.iter()) {
                    let arg = match arg.emit_ir(gen)? {
                        Value::Func(_) => todo!(),
                        Value::Bool(v) => v.into(),
                        Value::I32(v) => v.into(),
                        Value::Str(v) => v.into(),
                        Value::Never => todo!(),
                        Value::Void => todo!(),
                    };
                    args.push(arg);
                }
                let val = gen
                    .builder
                    .build_direct_call(func.fn_ptr, &args, "tmp-call")
                    .unwrap();

                match val.try_as_basic_value().left() {
                    Some(BasicValueEnum::ArrayValue(v)) => todo!("{v}"),
                    Some(BasicValueEnum::IntValue(v)) => Ok(Value::I32(v)),
                    Some(BasicValueEnum::FloatValue(v)) => todo!("{v}"),
                    Some(BasicValueEnum::PointerValue(v)) => todo!("{v}"),
                    Some(BasicValueEnum::StructValue(v)) => todo!("{v}"),
                    Some(BasicValueEnum::VectorValue(v)) => todo!("{v}"),
                    None => Ok(Value::Void),
                }
            }
        }
    }
}

impl<L: EmitIr, R: EmitIr> EmitIr for (L, R) {
    type Val = (L::Val, R::Val);

    fn emit_ir(&self, gen: &mut ModuleGen) -> Result<Self::Val> {
        Ok((self.0.emit_ir(gen)?, self.1.emit_ir(gen)?))
    }
}

//

pub enum ConstValue {
    // TODO: LitInt(i128)
    I32(i32),
    Never,
    None,
}

impl EmitIr for ConstValue {
    type Val = Value;

    fn emit_ir(&self, gen: &mut ModuleGen) -> Result<Self::Val> {
        Ok(match self {
            ConstValue::I32(i) => Value::I32(gen.ctx.i32_type().const_int(*i as _, false)),
            ConstValue::Never => Value::Never,
            ConstValue::None => Value::Void,
        })
    }
}

//

pub trait ConstEval {
    type Val;

    fn eval(&self) -> Result<Self::Val>;
}

impl ConstEval for ast::Expr {
    type Val = ConstValue;

    fn eval(&self) -> Result<Self::Val> {
        match &self.expr {
            ast::AnyExpr::Block(block) => block.eval(),
            ast::AnyExpr::LitInt(i) => Ok(ConstValue::I32(i.value as _)),
            ast::AnyExpr::LitStr(_) => todo!(),
            ast::AnyExpr::Load(_) => todo!(),
            ast::AnyExpr::Func(_) => todo!(),
            ast::AnyExpr::Binary { op, sides } => {
                let (lhs, rhs) = sides.eval()?;
                match (*op, lhs, rhs) {
                    (BinaryOp::Mul, ConstValue::I32(lhs), ConstValue::I32(rhs)) => {
                        Ok(ConstValue::I32(lhs.wrapping_mul(rhs)))
                    }
                    (BinaryOp::Div, ConstValue::I32(lhs), ConstValue::I32(rhs)) => {
                        Ok(ConstValue::I32(lhs.wrapping_div(rhs)))
                    }
                    (BinaryOp::Rem, ConstValue::I32(lhs), ConstValue::I32(rhs)) => {
                        Ok(ConstValue::I32(lhs.wrapping_rem(rhs)))
                    }

                    (BinaryOp::Add, ConstValue::I32(lhs), ConstValue::I32(rhs)) => {
                        Ok(ConstValue::I32(lhs.wrapping_add(rhs)))
                    }
                    (BinaryOp::Sub, ConstValue::I32(lhs), ConstValue::I32(rhs)) => {
                        Ok(ConstValue::I32(lhs.wrapping_sub(rhs)))
                    }
                    _ => todo!(),
                }
            }
            ast::AnyExpr::Call(_) => todo!(),
        }
    }
}

impl ConstEval for ast::Block {
    type Val = ConstValue;

    fn eval(&self) -> Result<Self::Val> {
        let mut last = None;
        for stmt in self.stmts.iter() {
            match stmt.eval()? {
                ConstValue::Never => return Ok(ConstValue::Never),
                v => last = Some(v),
            };
        }

        if !self.auto_return {
            return Ok(ConstValue::None);
        }

        Ok(last.unwrap_or(ConstValue::None))
    }
}

impl ConstEval for ast::Stmt {
    type Val = ConstValue;

    fn eval(&self) -> Result<Self::Val> {
        match self {
            ast::Stmt::Init(_) => todo!(),
            ast::Stmt::Set(_) => todo!(),
            ast::Stmt::Cond(_) => todo!(),
            ast::Stmt::Loop(_) => todo!(),
            ast::Stmt::Expr(expr) => expr.expr.eval(),
            ast::Stmt::Return(_) => todo!(),
        }
    }
}

impl<L: ConstEval, R: ConstEval> ConstEval for (L, R) {
    type Val = (L::Val, R::Val);

    fn eval(&self) -> Result<Self::Val> {
        Ok((self.0.eval()?, self.1.eval()?))
    }
}

//

fn context() -> &'static Context {
    thread_local! {
        static CTX: &'static Context = Box::leak(Box::new(Context::create()));
    }

    CTX.with(|c| *c)
}*/
