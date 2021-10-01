use rslint_parser::AstNode;

// 1. track variables introduced in template
// currently only v-for and v-slot
// 2. prefix expression
use super::collect_entities::is_hoisted_asset;
use super::{BaseInfo, CorePassExt, Scope, TransformOption};
use crate::cast;
use crate::converter::{BindingTypes, JsExpr as Js};
use crate::flags::{RuntimeHelper as RH, StaticLevel};
use crate::util::{is_global_allow_listed, is_simple_identifier, rslint, VStr};

pub struct ExpressionProcessor<'a, 'b> {
    pub option: &'b TransformOption<'a>,
}

impl<'a, 'b> CorePassExt<BaseInfo<'a>, Scope<'a>> for ExpressionProcessor<'a, 'b> {
    fn enter_fn_param(&mut self, p: &mut Js<'a>, shared: &mut Scope<'a>) {
        process_fn_param(p);
        match p {
            Js::Param(id) => shared.add_identifier(id),
            Js::Compound(ids) => {
                for id in only_param_ids(ids) {
                    shared.add_identifier(id);
                }
            }
            _ => panic!("only Js::Param is legal"),
        }
    }
    fn exit_fn_param(&mut self, p: &mut Js<'a>, shared: &mut Scope<'a>) {
        match p {
            Js::Param(id) => shared.remove_identifier(id),
            Js::Compound(ids) => {
                for id in only_param_ids(ids) {
                    shared.remove_identifier(id);
                }
            }
            _ => panic!("only Js::Param is legal"),
        };
    }
    // only transform expression after its' sub-expression is transformed
    // e.g. compound/array/call expression
    fn exit_js_expr(&mut self, e: &mut Js<'a>, shared: &mut Scope<'a>) {
        self.process_expression(e, shared);
    }
}

impl<'a, 'b> ExpressionProcessor<'a, 'b> {
    fn process_expression(&self, e: &mut Js<'a>, scope: &Scope) {
        if !self.option.prefix_identifier {
            return;
        }
        // hoisted component/directive does not need prefixing
        if is_hoisted_asset(e).is_some() {
            return;
        }
        // complex expr will be handled recusively in transformer
        if !matches!(e, Js::Simple(..)) {
            return;
        }
        if self.process_expr_fast(e, scope) {
            return;
        }
        self.process_with_js_parser(e, scope);
    }

    /// prefix _ctx without parsing JS
    fn process_expr_fast(&self, e: &mut Js<'a>, scope: &Scope) -> bool {
        let (v, level) = match e {
            Js::Simple(v, level) => (v, level),
            _ => return false,
        };
        if !is_simple_identifier(*v) {
            return false;
        }
        let raw_exp = v.raw;
        let is_scope_reference = scope.has_identifier(v);
        let is_allowed_global = is_global_allow_listed(raw_exp);
        let is_literal = matches!(raw_exp, "true" | "false" | "null" | "this");
        if !is_scope_reference && !is_allowed_global && !is_literal {
            // const bindings from setup can skip patching but cannot be hoisted
            // NB: this only applies to simple expression. e.g :prop="constBind()"
            let bindings = &self.option.binding_metadata;
            let lvl = match bindings.get(raw_exp) {
                Some(BindingTypes::SetupConst) => StaticLevel::CanSkipPatch,
                _ => *level,
            };
            *e = self.rewrite_identifier(*v, lvl, CtxType::NoWrite);
        } else if !is_scope_reference {
            *level = if is_literal {
                StaticLevel::CanStringify
            } else {
                StaticLevel::CanHoist
            };
        }
        true
    }

    fn process_with_js_parser(&self, e: &mut Js<'a>, scope: &Scope) {
        let (v, level) = match e {
            Js::Simple(v, level) => (v, level),
            _ => return,
        };
        let raw = v.raw;
        let broken_atoms = self.break_down_complex_expression(raw, scope);
        *e = self.reunite_atoms(raw, broken_atoms);
    }
    fn rewrite_identifier(&self, raw: VStr<'a>, level: StaticLevel, ctx: CtxType<'a>) -> Js<'a> {
        let binding = self.option.binding_metadata.get(&raw.raw);
        if let Some(bind) = binding {
            if self.option.inline {
                rewrite_inline_identifier(raw, level, bind, ctx)
            } else {
                bind.get_js_prop(raw, level)
            }
        } else {
            debug_assert!(level == StaticLevel::NotStatic);
            Js::simple(*raw.clone().prefix_ctx())
        }
    }

    fn break_down_complex_expression(&self, raw: &'a str, scope: &Scope) -> Vec<Atom<'a>> {
        let expr = rslint::parse_js_expr(raw);
        let expr = match expr {
            Some(exp) => exp,
            None => todo!("add error handler"),
        };
        let inline = self.option.inline;
        let mut atoms = vec![];
        use std::ops::Range;
        rslint::walk_free_variables(expr, |fv| {
            let id_text = fv.text();
            if is_global_allow_listed(&id_text) || id_text == "require" {
                return;
            }
            let range = Range::from(fv.range());
            let id_str = VStr::raw(&raw[range.clone()]);
            // skip id defined in the template scope
            if scope.has_identifier(&id_str) {
                return;
            }
            atoms.push(Atom {
                range,
                id_str,
                ctx_type: if inline { todo!() } else { CtxType::NoWrite },
            })
        });
        atoms.sort_by_key(|r| r.range.start);
        atoms
    }

    fn reunite_atoms(&self, raw: &'a str, atoms: Vec<Atom<'a>>) -> Js<'a> {
        let mut inner = vec![];
        let mut last = 0;
        for atom in atoms {
            let range = atom.range;
            if last < range.start {
                let comp = Js::Src(&raw[last..range.start]);
                inner.push(comp);
            }
            last = range.end;
            let rewritten =
                self.rewrite_identifier(atom.id_str, StaticLevel::NotStatic, atom.ctx_type);
            inner.push(rewritten);
        }
        if last < raw.len() {
            inner.push(Js::Src(&raw[last..]));
        }
        Js::Compound(inner)
    }
}

// This implementation assumes that broken param expression has only two kinds subexpr:
// 1. param identifiers represented by Js::Param
// 2. expression in default binding that has been prefixed
fn only_param_ids<'a, 'b>(ids: &'b [Js<'a>]) -> impl Iterator<Item = &'a str> + 'b {
    ids.iter().filter_map(|id| match id {
        Js::Param(p) => Some(*p),
        Js::Src(_) => None,
        Js::Simple(..) => None,
        _ => panic!("Illegal sub expr kind in param."),
    })
}

struct Atom<'a> {
    range: std::ops::Range<usize>,
    id_str: VStr<'a>,
    ctx_type: CtxType<'a>,
}

enum CtxType<'a> {
    /// ref = value, ref += value
    Assign(Js<'a>),
    /// ref++, ++ref, ...
    Update(bool, Js<'a>),
    /// ({x}) = y
    Destructure,
    /// No reactive var writing
    NoWrite,
}

// parse expr as function params:
// 1. breaks down binding pattern e.g. [a, b, c] => identifiers a, b and c
// 2. patch default parameter like v-slot="a = 123" -> (a = 123)
fn process_fn_param(p: &mut Js) {
    let v = cast!(p, Js::Param);
    if is_simple_identifier(VStr::raw(v)) {
        // nothing LOL
        return;
    }
    todo!()
}

fn rewrite_inline_identifier<'a>(
    raw: VStr<'a>,
    level: StaticLevel,
    bind: &BindingTypes,
    ctx: CtxType<'a>,
) -> Js<'a> {
    use BindingTypes as BT;
    debug_assert!(level == StaticLevel::NotStatic || bind == &BT::SetupConst);
    let expr = move || Js::Simple(raw, level);
    let dot_value = Js::Compound(vec![expr(), Js::Src(".value")]);
    match bind {
        BT::SetupConst => expr(),
        BT::SetupRef => dot_value,
        BT::SetupMaybeRef => {
            // const binding that may or may not be ref
            // if it's not a ref, then assignments don't make sense -
            // so we ignore the non-ref assignment case and generate code
            // that assumes the value to be a ref for more efficiency
            if !matches!(ctx, CtxType::NoWrite) {
                dot_value
            } else {
                Js::Call(RH::Unref, vec![expr()])
            }
        }
        BT::SetupLet => rewrite_setup_let(ctx, expr, dot_value),
        BT::Props => Js::Compound(vec![Js::Src("__props."), expr()]),
        BT::Data | BT::Options => Js::Compound(vec![Js::Src("_ctx."), expr()]),
    }
}

fn rewrite_setup_let<'a, E>(ctx: CtxType<'a>, expr: E, dot_value: Js<'a>) -> Js<'a>
where
    E: Fn() -> Js<'a>,
{
    match ctx {
        CtxType::Assign(assign) => Js::Compound(vec![
            Js::Call(RH::IsRef, vec![expr()]),
            Js::Src("? "),
            dot_value,
            assign.clone(),
            Js::Src(": "),
            expr(),
            assign,
        ]),
        CtxType::Update(is_pre, op) => {
            let mut v = vec![Js::Call(RH::IsRef, vec![expr()])];
            v.push(Js::Src("? "));
            let push = |v: &mut Vec<_>, val, op| {
                if is_pre {
                    v.extend([op, val]);
                } else {
                    v.extend([val, op]);
                }
            };
            push(&mut v, dot_value, op.clone());
            v.push(Js::Src(": "));
            push(&mut v, expr(), op);
            Js::Compound(v)
        }
        CtxType::Destructure => {
            // TODO let binding in a destructure assignment - it's very tricky to
            // handle both possible cases here without altering the original
            // structure of the code, so we just assume it's not a ref here for now
            expr()
        }
        CtxType::NoWrite => Js::Call(RH::Unref, vec![expr()]),
    }
}

#[cfg(test)]
mod test {
    use super::super::{
        test::{base_convert, transformer_ext},
        BaseRoot, TransformOption, Transformer,
    };
    use super::*;
    use crate::cast;
    use crate::converter::{BaseIR, IRNode};

    fn transform(s: &str) -> BaseRoot {
        let option = TransformOption {
            prefix_identifier: true,
            ..Default::default()
        };
        let mut ir = base_convert(s);
        let mut exp = ExpressionProcessor { option: &option };
        let a: &mut [&mut dyn CorePassExt<_, _>] = &mut [&mut exp];
        let mut transformer = transformer_ext(a);
        transformer.transform(&mut ir);
        ir
    }
    fn first_child(ir: BaseRoot) -> BaseIR {
        ir.body.into_iter().next().unwrap()
    }

    #[test]
    fn test_interpolation_prefix() {
        let ir = transform("{{test}}");
        let text = cast!(first_child(ir), IRNode::TextCall);
        let text = match &text.texts[0] {
            Js::Call(_, r) => &r[0],
            _ => panic!("wrong interpolation"),
        };
        let expr = cast!(text, Js::Simple);
        assert_eq!(expr.into_string(), "_ctx.test");
    }
    #[test]
    fn test_prop_prefix() {
        let ir = transform("<p :test='a'/>");
        let vn = cast!(first_child(ir), IRNode::VNodeCall);
        let props = vn.props.unwrap();
        let props = cast!(props, Js::Props);
        let key = cast!(&props[0].0, Js::StrLit);
        assert_eq!(key.into_string(), "test");
        let expr = cast!(&props[0].1, Js::Simple);
        assert_eq!(expr.into_string(), "_ctx.a");
    }
    #[test]
    fn test_v_bind_prefix() {
        let ir = transform("<p v-bind='b'/>");
        let vn = cast!(&ir.body[0], IRNode::VNodeCall);
        let props = vn.props.as_ref().unwrap();
        let expr = cast!(props, Js::Simple);
        assert_eq!(expr.into_string(), "_ctx.b");
    }
    #[test]
    fn test_prefix_v_for() {
        let ir = transform("<p v-for='a in b'/>");
        let v_for = cast!(first_child(ir), IRNode::For);
        let b = cast!(v_for.source, Js::Simple);
        let a = cast!(v_for.parse_result.value, Js::Param);
        assert_eq!(a, "a");
        assert_eq!(b.into_string(), "_ctx.b");
    }
    #[test]
    fn test_complex_expression() {
        let ir = transform("{{a + b}}");
        let text = cast!(first_child(ir), IRNode::TextCall);
        let text = match &text.texts[0] {
            Js::Call(_, r) => &r[0],
            _ => panic!("wrong interpolation"),
        };
        let expr = cast!(text, Js::Compound);
        let a = cast!(expr[0], Js::Simple);
        let b = cast!(expr[2], Js::Simple);
        assert_eq!(a.into_string(), "_ctx.a");
        assert_eq!(b.into_string(), "_ctx.b");
    }
}
