use std::cmp::max;
use std::collections::HashMap;

use dora_parser::ast::visit::*;
use dora_parser::ast::Expr::*;
use dora_parser::ast::Stmt::*;
use dora_parser::ast::*;

use crate::cpu::*;
use crate::mem;
use crate::semck::specialize::{specialize_for_call_type, specialize_type};
use crate::ty::{BuiltinType, TypeList, TypeParamId};
use crate::vm::{
    Arg, CallSite, CallType, Fct, FctId, FctKind, FctParent, FctSrc, Intrinsic, NodeMap, Store,
    TraitId, VarId, VM,
};

pub fn generate<'a, 'ast: 'a>(
    vm: &'a VM<'ast>,
    fct: &Fct<'ast>,
    src: &'a FctSrc,
    jit_info: &'a mut JitInfo<'ast>,
    cls_type_params: &TypeList,
    fct_type_params: &TypeList,
) {
    let start = if fct.has_self() { 1 } else { 0 };

    if let FctParent::Class(cls_id) = fct.parent {
        let cls = vm.classes.idx(cls_id);
        let cls = cls.read();
        assert_eq!(cls_type_params.len(), cls.type_params.len());
    } else {
        assert_eq!(cls_type_params.len(), 0);
    }

    assert_eq!(fct.type_params.len(), fct_type_params.len());

    for ty in cls_type_params.iter() {
        assert!(ty.is_concrete_type(vm));
    }

    for ty in fct_type_params.iter() {
        assert!(ty.is_concrete_type(vm));
    }

    let mut ig = InfoGenerator {
        vm,
        fct,
        ast: fct.ast,
        src,
        jit_info,

        stacksize: 0,

        param_offset: PARAM_OFFSET,
        leaf: true,
        eh_return_value: None,
        eh_status: None,

        param_reg_idx: start,
        param_freg_idx: 0,

        cls_type_params,
        fct_type_params,
    };

    ig.generate();
}

pub struct JitInfo<'ast> {
    pub stacksize: i32,               // size of local variables on stack
    pub leaf: bool,                   // false if fct calls other functions
    pub eh_return_value: Option<i32>, // stack slot for return value storage

    pub map_stores: NodeMap<Store>,
    pub map_csites: NodeMap<CallSite<'ast>>,
    pub map_offsets: NodeMap<i32>,
    pub map_var_offsets: HashMap<VarId, i32>,
    pub map_var_types: HashMap<VarId, BuiltinType>,
    pub map_intrinsics: NodeMap<Intrinsic>,
    pub map_fors: NodeMap<ForInfo<'ast>>,
    pub map_templates: NodeMap<TemplateJitInfo<'ast>>,
}

impl<'ast> JitInfo<'ast> {
    pub fn get_store(&self, id: NodeId) -> Store {
        match self.map_stores.get(id) {
            Some(store) => *store,
            None => Store::Reg,
        }
    }

    pub fn stacksize(&self) -> i32 {
        self.stacksize
    }

    pub fn offset(&self, var_id: VarId) -> i32 {
        *self
            .map_var_offsets
            .get(&var_id)
            .expect("no offset found for var")
    }

    pub fn ty(&self, var_id: VarId) -> BuiltinType {
        *self
            .map_var_types
            .get(&var_id)
            .expect("no type found for var")
    }

    pub fn new() -> JitInfo<'ast> {
        JitInfo {
            stacksize: 0,
            leaf: false,
            eh_return_value: None,

            map_stores: NodeMap::new(),
            map_csites: NodeMap::new(),
            map_offsets: NodeMap::new(),
            map_var_offsets: HashMap::new(),
            map_var_types: HashMap::new(),
            map_intrinsics: NodeMap::new(),
            map_fors: NodeMap::new(),
            map_templates: NodeMap::new(),
        }
    }
}

struct InfoGenerator<'a, 'ast: 'a> {
    vm: &'a VM<'ast>,
    fct: &'a Fct<'ast>,
    src: &'a FctSrc,
    ast: &'ast Function,
    jit_info: &'a mut JitInfo<'ast>,

    stacksize: i32,

    eh_return_value: Option<i32>,
    eh_status: Option<i32>,
    param_offset: i32,
    leaf: bool,

    param_reg_idx: usize,
    param_freg_idx: usize,

    cls_type_params: &'a TypeList,
    fct_type_params: &'a TypeList,
}

impl<'a, 'ast> Visitor<'ast> for InfoGenerator<'a, 'ast> {
    fn visit_param(&mut self, p: &'ast Param) {
        let var = *self.src.map_vars.get(p.id).unwrap();
        let ty = self.src.vars[var].ty;
        let ty = self.specialize_type(ty);
        self.jit_info.map_var_types.insert(var, ty);

        let is_float = ty.is_float();

        // only some parameters are passed in registers
        // these registers need to be stored into local variables
        if is_float && self.param_freg_idx < FREG_PARAMS.len() {
            self.reserve_stack_for_var(var);
            self.param_freg_idx += 1;
        } else if !is_float && self.param_reg_idx < REG_PARAMS.len() {
            self.reserve_stack_for_var(var);
            self.param_reg_idx += 1;

        // the rest of the parameters are already stored on the stack
        // just use the current offset
        } else {
            let var = &self.src.vars[var];
            self.jit_info
                .map_var_offsets
                .insert(var.id, self.param_offset);

            // determine next `param_offset`
            self.param_offset = next_param_offset(self.param_offset, var.ty);
        }
    }

    fn visit_stmt(&mut self, s: &'ast Stmt) {
        match s {
            &StmtVar(ref var) => {
                let var = *self.src.map_vars.get(var.id).unwrap();
                self.reserve_stack_for_var(var);
            }

            &StmtDo(ref r#try) => {
                self.reserve_stmt_do(r#try);
            }

            &StmtFor(ref sfor) => {
                self.reserve_stmt_for(sfor);
            }

            _ => {}
        }

        visit::walk_stmt(self, s);
    }

    fn visit_expr(&mut self, e: &'ast Expr) {
        match *e {
            ExprCall(ref expr) => self.expr_call(expr),
            ExprDelegation(ref expr) => self.expr_delegation(expr),
            ExprBin(ref expr) => self.expr_bin(expr),
            ExprUn(ref expr) => self.expr_un(expr),
            ExprConv(ref expr) => self.expr_conv(expr),
            ExprTypeParam(_) => unreachable!(),
            ExprTemplate(ref expr) => self.expr_template(expr),

            _ => visit::walk_expr(self, e),
        }
    }
}

impl<'a, 'ast> InfoGenerator<'a, 'ast> {
    fn generate(&mut self) {
        if self.fct.has_self() {
            self.reserve_stack_for_self();
        }

        self.visit_fct(self.ast);

        self.jit_info.stacksize = mem::align_i32(self.stacksize, 16);
        self.jit_info.leaf = self.leaf;
        self.jit_info.eh_return_value = self.eh_return_value;
    }

    fn reserve_stmt_do(&mut self, r#try: &'ast StmtDoType) {
        let ret = self.specialize_type(self.fct.return_type);

        if !ret.is_unit() {
            self.eh_return_value = Some(
                self.eh_return_value
                    .unwrap_or_else(|| self.reserve_stack_slot(ret)),
            );
        }

        // we also need space for catch block parameters
        for catch in &r#try.catch_blocks {
            let var = *self.src.map_vars.get(catch.id).unwrap();
            self.reserve_stack_for_var(var);
        }

        if r#try.finally_block.is_some() {
            let offset = self.reserve_stack_slot(BuiltinType::Ptr);
            self.jit_info.map_offsets.insert(r#try.id, offset);
        }
    }

    fn reserve_stmt_for(&mut self, stmt: &'ast StmtForType) {
        let for_type_info = self.src.map_fors.get(stmt.id).unwrap();

        // reserve stack slot for iterated value
        let var = *self.src.map_vars.get(stmt.id).unwrap();
        self.reserve_stack_for_var(var);

        // reserve stack slot for iterator
        let offset = self.reserve_stack_slot(for_type_info.iterator_type);
        self.jit_info.map_offsets.insert(stmt.id, offset);

        // build makeIterator() call
        let object_type = self.ty(stmt.expr.id());
        let ctype = CallType::Method(object_type, for_type_info.make_iterator, TypeList::empty());
        let args = vec![Arg::Expr(&stmt.expr, BuiltinType::Unit, 0)];
        let make_iterator = self.build_call_site(&ctype, for_type_info.make_iterator, args);

        // build hasNext() call
        let ctype = CallType::Method(
            for_type_info.iterator_type,
            for_type_info.has_next,
            TypeList::empty(),
        );
        let args = vec![Arg::Stack(offset, BuiltinType::Unit, 0)];
        let has_next = self.build_call_site(&ctype, for_type_info.has_next, args);

        // build next() call
        let ctype = CallType::Method(
            for_type_info.iterator_type,
            for_type_info.next,
            TypeList::empty(),
        );
        let args = vec![Arg::Stack(offset, BuiltinType::Unit, 0)];
        let next = self.build_call_site(&ctype, for_type_info.next, args);

        self.jit_info.map_fors.insert(
            stmt.id,
            ForInfo {
                make_iterator,
                has_next,
                next,
            },
        );
    }

    fn reserve_stack_for_self(&mut self) {
        let ty = match self.fct.parent {
            FctParent::Class(clsid) => {
                let cls = self.vm.classes.idx(clsid);
                let cls = cls.read();

                cls.ty
            }

            FctParent::Impl(impl_id) => {
                let ximpl = self.vm.impls[impl_id].read();

                let cls = self.vm.classes.idx(ximpl.cls_id());
                let cls = cls.read();

                cls.ty
            }

            _ => unreachable!(),
        };

        let offset = self.reserve_stack_slot(ty);

        let id = self.src.var_self().id;
        self.jit_info.map_var_offsets.insert(id, offset);
    }

    fn reserve_stack_for_var(&mut self, id: VarId) -> i32 {
        let ty = self.src.vars[id].ty;
        let ty = self.specialize_type(ty);
        let offset = self.reserve_stack_slot(ty);

        self.jit_info.map_var_offsets.insert(id, offset);
        self.jit_info.map_var_types.insert(id, ty);

        offset
    }

    fn expr_conv(&mut self, e: &'ast ExprConvType) {
        self.visit_expr(&e.object);
        let is_valid = self.src.map_convs.get(e.id).unwrap().valid;

        if !e.is && !is_valid {
            self.reserve_temp_for_node(&e.object);
        }
    }

    fn get_intrinsic(&self, id: NodeId) -> Option<Intrinsic> {
        let call_type = self.src.map_calls.get(id).unwrap();

        if let Some(intrinsic) = call_type.to_intrinsic() {
            return Some(intrinsic);
        }

        let fid = call_type.fct_id().unwrap();

        // the function we compile right now is never an intrinsic
        if self.fct.id == fid {
            return None;
        }

        let fct = self.vm.fcts.idx(fid);
        let fct = fct.read();

        match fct.kind {
            FctKind::Builtin(intr) => Some(intr),
            _ => None,
        }
    }

    fn expr_call(&mut self, expr: &'ast ExprCallType) {
        if let Some(intrinsic) = self.get_intrinsic(expr.id) {
            self.reserve_args_call(expr);
            self.jit_info.map_intrinsics.insert(expr.id, intrinsic);

            match intrinsic {
                Intrinsic::Assert => {
                    let offset = self.reserve_stack_slot(BuiltinType::Ptr);
                    let cls_id = self.vm.vips.error_class;
                    let cls = self.vm.classes.idx(cls_id);
                    let cls = cls.read();
                    let selfie_offset = self.reserve_stack_slot(cls.ty);
                    let args = vec![
                        Arg::SelfieNew(cls.ty, selfie_offset),
                        Arg::Stack(offset, BuiltinType::Ptr, 0),
                    ];
                    self.universal_call(expr.id, args, cls.constructor);
                }
                _ => {}
            };
            return;
        }

        let call_type = self.src.map_calls.get(expr.id).unwrap().clone();

        let mut args = expr
            .args
            .iter()
            .map(|arg| Arg::Expr(arg, BuiltinType::Unit, 0))
            .collect::<Vec<_>>();

        let fct_id: FctId;

        match *call_type {
            CallType::Ctor(_, fid, _) | CallType::CtorNew(_, fid, _) => {
                let ty = self.ty(expr.id);
                let arg = if call_type.is_ctor() {
                    Arg::Selfie(ty, 0)
                } else {
                    Arg::SelfieNew(ty, 0)
                };

                args.insert(0, arg);

                fct_id = fid;
            }

            CallType::Method(_, fid, _) => {
                let object = expr.object().unwrap();
                args.insert(0, Arg::Expr(object, BuiltinType::Unit, 0));

                fct_id = fid;
            }

            CallType::Fct(fid, _, _) => {
                fct_id = fid;
            }

            CallType::Expr(_, fid) => {
                let object = &expr.callee;
                let ty = self.ty(object.id());
                args.insert(0, Arg::Expr(object, ty, 0));

                fct_id = fid;
            }

            CallType::TraitStatic(tp_id, trait_id, trait_fct_id) => {
                let list_id = match tp_id {
                    TypeParamId::Fct(list_id) => list_id,
                    TypeParamId::Class(_) => unimplemented!(),
                };

                let ty = self.fct_type_params[list_id.idx()];
                let cls_id = ty.cls_id(self.vm).expect("no cls_id for type");

                let cls = self.vm.classes.idx(cls_id);
                let cls = cls.read();

                let mut impl_fct_id: Option<FctId> = None;

                for &impl_id in &cls.impls {
                    let ximpl = self.vm.impls[impl_id].read();

                    if ximpl.trait_id != Some(trait_id) {
                        continue;
                    }

                    for &fid in &ximpl.methods {
                        let method = self.vm.fcts.idx(fid);
                        let method = method.read();

                        if method.impl_for == Some(trait_fct_id) {
                            impl_fct_id = Some(fid);
                            break;
                        }
                    }
                }

                fct_id = impl_fct_id.expect("no impl_fct_id found");
            }

            CallType::Trait(_, _) => unimplemented!(),
            CallType::Intrinsic(_) => unreachable!(),
        }

        let fct = self.vm.fcts.idx(fct_id);
        let fct = fct.read();

        let callee_id = if fct.kind.is_definition() {
            let trait_id = fct.trait_id();
            let object_type = match *call_type {
                CallType::Method(ty, _, _) => ty,
                _ => unreachable!(),
            };

            let object_type = self.specialize_type(object_type);

            self.find_trait_impl(fct_id, trait_id, object_type)
        } else {
            fct_id
        };

        let callee = self.vm.fcts.idx(callee_id);
        let callee = callee.read();

        if let FctKind::Builtin(intrinsic) = callee.kind {
            self.reserve_args_call(expr);
            self.jit_info.map_intrinsics.insert(expr.id, intrinsic);
            return;
        }

        self.universal_call(expr.id, args, Some(callee_id));
    }

    fn reserve_args_call(&mut self, expr: &'ast ExprCallType) {
        for arg in &expr.args {
            self.visit_expr(arg);
            self.reserve_temp_for_node(arg);
        }

        let call_type = self.src.map_calls.get(expr.id).unwrap();

        if call_type.is_method() {
            let object = expr.object().unwrap();
            self.visit_expr(object);
            self.reserve_temp_for_node(object);
        } else if call_type.is_expr() {
            self.visit_expr(&expr.callee);
            self.reserve_temp_for_node(&expr.callee);
        }
    }

    fn find_trait_impl(&self, fct_id: FctId, trait_id: TraitId, object_type: BuiltinType) -> FctId {
        let cls_id = object_type.cls_id(self.vm).unwrap();
        let cls = self.vm.classes.idx(cls_id);
        let cls = cls.read();

        for &impl_id in &cls.impls {
            let ximpl = self.vm.impls[impl_id].read();

            if ximpl.trait_id() != trait_id {
                continue;
            }

            for &mtd_id in &ximpl.methods {
                let mtd = self.vm.fcts.idx(mtd_id);
                let mtd = mtd.read();

                if mtd.impl_for == Some(fct_id) {
                    return mtd_id;
                }
            }
        }

        panic!("no impl found for generic trait call")
    }

    fn expr_delegation(&mut self, expr: &'ast ExprDelegationType) {
        let mut args = expr
            .args
            .iter()
            .map(|arg| Arg::Expr(arg, BuiltinType::Unit, 0))
            .collect::<Vec<_>>();

        let cls = self.ty(expr.id);
        args.insert(0, Arg::Selfie(cls, 0));

        self.universal_call(expr.id, args, None);
    }

    fn universal_call(&mut self, id: NodeId, args: Vec<Arg<'ast>>, callee_id: Option<FctId>) {
        let call_type = self.src.map_calls.get(id).unwrap().clone();

        let callee_id = if let Some(callee_id) = callee_id {
            callee_id
        } else {
            call_type.fct_id().unwrap()
        };

        let csite = self.build_call_site(&*call_type, callee_id, args);

        // remember args
        self.jit_info.map_csites.insert_or_replace(id, csite);
    }

    fn build_call_site(
        &mut self,
        call_type: &CallType,
        callee_id: FctId,
        args: Vec<Arg<'ast>>,
    ) -> CallSite<'ast> {
        // function invokes another function
        self.leaf = false;

        let callee = self.vm.fcts.idx(callee_id);
        let callee = callee.read();

        let (args, return_type, super_call) =
            self.determine_call_args_and_types(&*call_type, &*callee, args);
        let (cls_type_params, fct_type_params) = self.determine_call_type_params(&*call_type);

        let argsize = self.determine_call_stack(&args);

        CallSite {
            callee: callee_id,
            args,
            argsize,
            cls_type_params,
            fct_type_params,
            super_call,
            return_type,
        }
    }

    fn determine_call_args_and_types(
        &mut self,
        call_type: &CallType,
        callee: &Fct<'ast>,
        args: Vec<Arg<'ast>>,
    ) -> (Vec<Arg<'ast>>, BuiltinType, bool) {
        let mut super_call = false;

        assert!(callee.params_with_self().len() == args.len());

        let args = args
            .iter()
            .enumerate()
            .map(|(ind, arg)| {
                let ty = callee.params_with_self()[ind];
                let ty = self.specialize_type(specialize_for_call_type(call_type, ty, self.vm));
                let offset = self.reserve_stack_slot(ty);

                match *arg {
                    Arg::Expr(ast, _, _) => {
                        if ind == 0 && ast.is_super() {
                            super_call = true;
                        }

                        Arg::Expr(ast, ty, offset)
                    }

                    Arg::Stack(soffset, _, _) => Arg::Stack(soffset, ty, offset),
                    Arg::SelfieNew(cid, _) => Arg::SelfieNew(cid, offset),
                    Arg::Selfie(cid, _) => Arg::Selfie(cid, offset),
                }
            })
            .collect::<Vec<_>>();

        let return_type = self.specialize_type(specialize_for_call_type(
            call_type,
            callee.return_type,
            self.vm,
        ));

        (args, return_type, super_call)
    }

    fn determine_call_type_params(&mut self, call_type: &CallType) -> (TypeList, TypeList) {
        let cls_type_params;
        let fct_type_params;

        match *call_type {
            CallType::Ctor(_, _, ref type_params) | CallType::CtorNew(_, _, ref type_params) => {
                cls_type_params = type_params.clone();
                fct_type_params = TypeList::empty();
            }

            CallType::Method(ty, _, ref type_params) => {
                let ty = self.specialize_type(ty);

                cls_type_params = ty.type_params(self.vm);
                fct_type_params = type_params.clone();
            }

            CallType::Fct(_, ref cls_tps, ref fct_tps) => {
                cls_type_params = cls_tps.clone();
                fct_type_params = fct_tps.clone();
            }

            CallType::Expr(ty, _) => {
                let ty = self.specialize_type(ty);

                cls_type_params = ty.type_params(self.vm);
                fct_type_params = TypeList::empty();
            }

            CallType::Trait(_, _) => unimplemented!(),

            CallType::TraitStatic(_, _, _) => {
                cls_type_params = TypeList::empty();
                fct_type_params = TypeList::empty();
            }

            CallType::Intrinsic(_) => unreachable!(),
        }

        (cls_type_params, fct_type_params)
    }

    fn determine_call_stack(&mut self, args: &[Arg<'ast>]) -> i32 {
        let mut reg_args: i32 = 0;
        let mut freg_args: i32 = 0;

        for arg in args {
            match *arg {
                Arg::Expr(ast, ty, _) => {
                    self.visit_expr(ast);

                    if ty.is_float() {
                        freg_args += 1;
                    } else {
                        reg_args += 1;
                    }
                }

                Arg::Stack(_, ty, _) | Arg::Selfie(ty, _) | Arg::SelfieNew(ty, _) => {
                    if ty.is_float() {
                        freg_args += 1;
                    } else {
                        reg_args += 1;
                    }
                }
            }
        }

        // some register are reserved on stack
        let args_on_stack = max(0, reg_args - REG_PARAMS.len() as i32)
            + max(0, freg_args - FREG_PARAMS.len() as i32);

        mem::align_i32(mem::ptr_width() * args_on_stack, 16)
    }

    fn expr_assign(&mut self, e: &'ast ExprBinType) {
        let call_type = self.src.map_calls.get(e.id);

        if call_type.is_some() {
            let call_expr = e.lhs.to_call().unwrap();

            let object = &call_expr.callee;
            let index = &call_expr.args[0];
            let value = &e.rhs;

            if let Some(intrinsic) = self.get_intrinsic(e.id) {
                self.visit_expr(object);
                self.visit_expr(index);
                self.visit_expr(value);

                self.reserve_temp_for_node(object);
                self.reserve_temp_for_node(index);

                let element_type = self.ty(object.id()).type_params(self.vm)[0];
                self.reserve_temp_for_node_with_type(e.rhs.id(), element_type);

                self.jit_info.map_intrinsics.insert(e.id, intrinsic);
            } else {
                let args = vec![
                    Arg::Expr(object, BuiltinType::Unit, 0),
                    Arg::Expr(index, BuiltinType::Unit, 0),
                    Arg::Expr(value, BuiltinType::Unit, 0),
                ];

                self.universal_call(e.id, args, None);
            }
        } else if e.lhs.is_ident() {
            self.visit_expr(&e.rhs);

            let lhs = e.lhs.to_ident().unwrap();
            let field = self.src.map_idents.get(lhs.id).unwrap().is_field();

            if field {
                self.reserve_temp_for_node_with_type(lhs.id, BuiltinType::Ptr);
            }
        } else {
            // e.lhs is a field
            let lhs = e.lhs.to_dot().unwrap();

            self.visit_expr(&lhs.lhs);
            self.visit_expr(&e.rhs);

            self.reserve_temp_for_node(&lhs.lhs);
            self.reserve_temp_for_node(&e.rhs);
        }
    }

    fn expr_bin(&mut self, expr: &'ast ExprBinType) {
        if expr.op.is_any_assign() {
            self.expr_assign(expr);
            return;
        }

        let lhs_ty = self.ty(expr.lhs.id());
        let rhs_ty = self.ty(expr.rhs.id());

        if expr.op == BinOp::Cmp(CmpOp::Is) || expr.op == BinOp::Cmp(CmpOp::IsNot) {
            self.visit_expr(&expr.lhs);
            self.visit_expr(&expr.rhs);

            self.reserve_temp_for_node_with_type(expr.lhs.id(), BuiltinType::Ptr);
        } else if expr.op == BinOp::Or || expr.op == BinOp::And {
            self.visit_expr(&expr.lhs);
            self.visit_expr(&expr.rhs);

        // no temporaries needed
        } else if let Some(intrinsic) = self.get_intrinsic(expr.id) {
            self.visit_expr(&expr.lhs);
            self.visit_expr(&expr.rhs);

            self.reserve_temp_for_node(&expr.lhs);
            self.jit_info.map_intrinsics.insert(expr.id, intrinsic);
        } else {
            let args = vec![
                Arg::Expr(&expr.lhs, lhs_ty, 0),
                Arg::Expr(&expr.rhs, rhs_ty, 0),
            ];
            let fid = self.src.map_calls.get(expr.id).unwrap().fct_id().unwrap();

            self.universal_call(expr.id, args, Some(fid));
        }
    }

    fn expr_un(&mut self, expr: &'ast ExprUnType) {
        if let Some(intrinsic) = self.get_intrinsic(expr.id) {
            // no temporaries needed
            self.visit_expr(&expr.opnd);
            self.jit_info.map_intrinsics.insert(expr.id, intrinsic);
        } else {
            let args = vec![Arg::Expr(&expr.opnd, BuiltinType::Unit, 0)];

            self.universal_call(expr.id, args, None);
        }
    }

    fn expr_template(&mut self, expr: &'ast ExprTemplateType) {
        let string_buffer_offset = self.reserve_stack_slot(BuiltinType::Ptr);
        let string_part_offset = self.reserve_stack_slot(BuiltinType::Ptr);

        // build StringBuffer::empty() call
        let fct_id = self.vm.vips.fct.string_buffer_empty;
        let ctype = CallType::Fct(fct_id, TypeList::empty(), TypeList::empty());
        let string_buffer_new = self.build_call_site(&ctype, fct_id, Vec::new());
        let mut part_infos = Vec::new();

        for part in &expr.parts {
            let mut object_offset = None;
            let mut to_string = None;

            if !part.is_lit_str() {
                self.visit_expr(part);
                let ty = self.ty(part.id());

                if ty.cls_id(self.vm) != Some(self.vm.vips.string_class) {
                    // build toString() call
                    let offset = self.reserve_stack_slot(ty);
                    object_offset = Some(offset);
                    let cls_id = ty.cls_id(self.vm).expect("no cls_id found for type");
                    let cls = self.vm.classes.idx(cls_id);
                    let cls = cls.read();
                    let name = self.vm.interner.intern("toString");
                    let to_string_id = cls
                        .find_trait_method(self.vm, self.vm.vips.stringable_trait, name, false)
                        .expect("toString() method not found");
                    let ctype = CallType::Method(ty, to_string_id, TypeList::empty());
                    let args = vec![Arg::Stack(offset, ty, 0)];
                    to_string = Some(self.build_call_site(&ctype, to_string_id, args));
                }
            }

            // build StringBuffer::append() call
            let fct_id = self.vm.vips.fct.string_buffer_append;
            let ty = BuiltinType::from_cls(self.vm.vips.cls.string_buffer, self.vm);
            let ctype = CallType::Method(ty, fct_id, TypeList::empty());
            let args = vec![
                Arg::Stack(string_buffer_offset, BuiltinType::Ptr, 0),
                Arg::Stack(string_part_offset, BuiltinType::Ptr, 0),
            ];
            let append = self.build_call_site(&ctype, fct_id, args);

            part_infos.push(TemplatePartJitInfo {
                object_offset,
                to_string,
                append,
            });
        }

        // build StringBuffer::toString() call
        let fct_id = self.vm.vips.fct.string_buffer_to_string;
        let ty = BuiltinType::from_cls(self.vm.vips.cls.string_buffer, self.vm);
        let ctype = CallType::Method(ty, fct_id, TypeList::empty());
        let args = vec![Arg::Stack(string_buffer_offset, BuiltinType::Ptr, 0)];
        let string_buffer_to_string = self.build_call_site(&ctype, fct_id, args);

        self.jit_info.map_templates.insert(
            expr.id,
            TemplateJitInfo {
                string_buffer_offset,
                string_part_offset,
                string_buffer_new,
                part_infos,
                string_buffer_to_string,
            },
        );
    }

    fn reserve_temp_for_node_id(&mut self, id: NodeId) -> i32 {
        let ty = self.ty(id);
        self.reserve_temp_for_node_with_type(id, ty)
    }

    fn reserve_temp_for_node(&mut self, expr: &Expr) -> i32 {
        let ty = self.ty(expr.id());
        self.reserve_temp_for_node_with_type(expr.id(), ty)
    }

    fn reserve_temp_for_ctor(&mut self, id: NodeId) -> i32 {
        self.reserve_temp_for_node_with_type(id, BuiltinType::Ptr)
    }

    fn reserve_temp_for_node_with_type(&mut self, id: NodeId, ty: BuiltinType) -> i32 {
        let offset = self.reserve_stack_slot(ty);

        self.jit_info
            .map_stores
            .insert_or_replace(id, Store::Temp(offset, ty));

        offset
    }

    fn reserve_stack_slot(&mut self, ty: BuiltinType) -> i32 {
        let (ty_size, ty_align) = if ty.is_nil() {
            (mem::ptr_width(), mem::ptr_width())
        } else {
            (ty.size(self.vm), ty.align(self.vm))
        };

        self.stacksize = mem::align_i32(self.stacksize, ty_align) + ty_size;

        -self.stacksize
    }

    fn ty(&self, id: NodeId) -> BuiltinType {
        let ty = self.src.ty(id);
        self.specialize_type(ty)
    }

    fn specialize_type(&self, ty: BuiltinType) -> BuiltinType {
        let result = specialize_type(self.vm, ty, &self.cls_type_params, &self.fct_type_params);
        assert!(result.is_concrete_type(self.vm));
        result
    }
}

#[derive(Clone)]
pub struct ForInfo<'ast> {
    pub make_iterator: CallSite<'ast>,
    pub has_next: CallSite<'ast>,
    pub next: CallSite<'ast>,
}

#[derive(Clone)]
pub struct TemplateJitInfo<'ast> {
    pub string_buffer_offset: i32,
    pub string_part_offset: i32,
    pub string_buffer_new: CallSite<'ast>,
    pub part_infos: Vec<TemplatePartJitInfo<'ast>>,
    pub string_buffer_to_string: CallSite<'ast>,
}

#[derive(Clone)]
pub struct TemplatePartJitInfo<'ast> {
    pub object_offset: Option<i32>,
    pub to_string: Option<CallSite<'ast>>,
    pub append: CallSite<'ast>,
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::os;
    use crate::test;
    use crate::vm::*;

    fn info<F>(code: &'static str, f: F)
    where
        F: FnOnce(&FctSrc, &JitInfo),
    {
        os::init_page_size();

        test::parse(code, |vm| {
            let fid = vm.fct_by_name("f").unwrap();
            let fct = vm.fcts.idx(fid);
            let fct = fct.read();
            let src = fct.src();
            let mut src = src.write();
            let mut jit_info = JitInfo::new();
            let empty = TypeList::empty();

            generate(vm, &fct, &mut src, &mut jit_info, &empty, &empty);

            f(&src, &jit_info);
        });
    }

    #[test]
    fn test_tempsize() {
        info("fun f() { 1+2*3; }", |_, jit_info| {
            assert_eq!(16, jit_info.stacksize);
        });
        info("fun f() { 2*3+4+5; }", |_, jit_info| {
            assert_eq!(16, jit_info.stacksize);
        });
        info("fun f() { 1+(2+(3+4)); }", |_, jit_info| {
            assert_eq!(16, jit_info.stacksize);
        })
    }

    #[test]
    fn test_tempsize_for_fct_call() {
        info(
            "fun f() { g(1,2,3,4,5,6); }
              fun g(a:Int, b:Int, c:Int, d:Int, e:Int, f:Int) {}",
            |_, jit_info| {
                assert_eq!(32, jit_info.stacksize);
            },
        );

        info(
            "fun f() { g(1,2,3,4,5,6,7,8); }
              fun g(a:Int, b:Int, c:Int, d:Int, e:Int, f:Int, g:Int, h:Int) {}",
            |_, jit_info| {
                assert_eq!(32, jit_info.stacksize);
            },
        );

        info(
            "fun f() { g(1,2,3,4,5,6,7,8)+(1+2); }
              fun g(a:Int, b:Int, c:Int, d:Int, e:Int, f:Int, g:Int, h:Int) -> Int {
                  return 0;
              }",
            |_, jit_info| {
                assert_eq!(48, jit_info.stacksize);
            },
        );
    }

    #[test]
    fn test_invocation_flag() {
        info("fun f() { g(); } fun g() { }", |_, jit_info| {
            assert!(!jit_info.leaf);
        });

        info("fun f() { }", |_, jit_info| {
            assert!(jit_info.leaf);
        });
    }

    #[test]
    fn test_param_offset() {
        info("fun f(a: Bool, b: Int) { let c = 1; }", |fct, jit_info| {
            assert_eq!(16, jit_info.stacksize);

            for (var, offset) in fct.vars.iter().zip(&[-1, -8, -12]) {
                assert_eq!(*offset, jit_info.offset(var.id));
            }
        });
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn test_params_over_6_offset() {
        info(
            "fun f(a: Int, b: Int, c: Int, d: Int,
                   e: Int, f: Int, g: Int, h: Int) {
                  let i : Int = 1;
              }",
            |fct, jit_info| {
                assert_eq!(32, jit_info.stacksize);
                let offsets = [-4, -8, -12, -16, -20, -24, 16, 24, -28];

                for (var, offset) in fct.vars.iter().zip(&offsets) {
                    assert_eq!(*offset, jit_info.offset(var.id));
                }
            },
        );
    }

    #[test]
    #[cfg(target_arch = "aarch64")]
    fn test_params_over_8_offset() {
        info(
            "fun f(a: Int, b: Int, c: Int, d: Int,
                   e: Int, f: Int, g: Int, h: Int,
                   i: Int, j: Int) {
                  let k : Int = 1;
              }",
            |fct, jit_info| {
                assert_eq!(36, jit_info.stacksize);
                let offsets = [-4, -8, -12, -16, -20, -24, -28, -32, 16, 24, -36];

                for (var, offset) in fct.vars.iter().zip(&offsets) {
                    assert_eq!(*offset, jit_info.offset(var.id));
                }
            },
        );
    }

    #[test]
    fn test_var_offset() {
        info(
            "fun f() { let a = true; let b = false; let c = 2; let d = \"abc\"; }",
            |fct, jit_info| {
                assert_eq!(16, jit_info.stacksize);

                for (var, offset) in fct.vars.iter().zip(&[-1, -2, -8, -16]) {
                    assert_eq!(*offset, jit_info.offset(var.id));
                }
            },
        );
    }
}
