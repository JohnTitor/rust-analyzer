//! Conversion code from/to Chalk.
use std::sync::Arc;

use log::debug;

use chalk_ir::{fold::shift::Shift, CanonicalVarKinds, GenericArg, TypeName};
use chalk_solve::rust_ir::{self, OpaqueTyDatumBound, WellKnownTrait};

use hir_def::{
    lang_item::{lang_attr, LangItemTarget},
    AssocContainerId, AssocItemId, HasModule, Lookup, TypeAliasId,
};
use ra_db::{salsa::InternKey, CrateId};

use super::{builtin, AssocTyValue, ChalkContext, Impl};
use crate::{
    db::HirDatabase,
    display::HirDisplay,
    method_resolution::{TyFingerprint, ALL_FLOAT_FPS, ALL_INT_FPS},
    utils::generics,
    CallableDef, DebruijnIndex, GenericPredicate, Substs, Ty, TypeCtor,
};
use mapping::{convert_where_clauses, generic_predicate_to_inline_bound, make_binders};

pub use self::interner::*;

pub(super) mod tls;
mod interner;
mod mapping;

pub(super) trait ToChalk {
    type Chalk;
    fn to_chalk(self, db: &dyn HirDatabase) -> Self::Chalk;
    fn from_chalk(db: &dyn HirDatabase, chalk: Self::Chalk) -> Self;
}

pub(super) fn from_chalk<T, ChalkT>(db: &dyn HirDatabase, chalk: ChalkT) -> T
where
    T: ToChalk<Chalk = ChalkT>,
{
    T::from_chalk(db, chalk)
}

impl<'a> chalk_solve::RustIrDatabase<Interner> for ChalkContext<'a> {
    fn associated_ty_data(&self, id: AssocTypeId) -> Arc<AssociatedTyDatum> {
        self.db.associated_ty_data(id)
    }
    fn trait_datum(&self, trait_id: TraitId) -> Arc<TraitDatum> {
        self.db.trait_datum(self.krate, trait_id)
    }
    fn adt_datum(&self, struct_id: AdtId) -> Arc<StructDatum> {
        self.db.struct_datum(self.krate, struct_id)
    }
    fn adt_repr(&self, _struct_id: AdtId) -> rust_ir::AdtRepr {
        unreachable!()
    }
    fn impl_datum(&self, impl_id: ImplId) -> Arc<ImplDatum> {
        self.db.impl_datum(self.krate, impl_id)
    }

    fn fn_def_datum(
        &self,
        fn_def_id: chalk_ir::FnDefId<Interner>,
    ) -> Arc<rust_ir::FnDefDatum<Interner>> {
        self.db.fn_def_datum(self.krate, fn_def_id)
    }

    fn impls_for_trait(
        &self,
        trait_id: TraitId,
        parameters: &[GenericArg<Interner>],
        binders: &CanonicalVarKinds<Interner>,
    ) -> Vec<ImplId> {
        debug!("impls_for_trait {:?}", trait_id);
        let trait_: hir_def::TraitId = from_chalk(self.db, trait_id);

        let ty: Ty = from_chalk(self.db, parameters[0].assert_ty_ref(&Interner).clone());

        fn binder_kind(ty: &Ty, binders: &CanonicalVarKinds<Interner>) -> Option<chalk_ir::TyKind> {
            if let Ty::Bound(bv) = ty {
                let binders = binders.as_slice(&Interner);
                if bv.debruijn == DebruijnIndex::INNERMOST {
                    if let chalk_ir::VariableKind::Ty(tk) = binders[bv.index].kind {
                        return Some(tk);
                    }
                }
            }
            None
        }

        let self_ty_fp = TyFingerprint::for_impl(&ty);
        let fps: &[TyFingerprint] = match binder_kind(&ty, binders) {
            Some(chalk_ir::TyKind::Integer) => &ALL_INT_FPS,
            Some(chalk_ir::TyKind::Float) => &ALL_FLOAT_FPS,
            _ => self_ty_fp.as_ref().map(std::slice::from_ref).unwrap_or(&[]),
        };

        // Note: Since we're using impls_for_trait, only impls where the trait
        // can be resolved should ever reach Chalk. `impl_datum` relies on that
        // and will panic if the trait can't be resolved.
        let in_deps = self.db.trait_impls_in_deps(self.krate);
        let in_self = self.db.trait_impls_in_crate(self.krate);
        let impl_maps = [in_deps, in_self];

        let id_to_chalk = |id: hir_def::ImplId| Impl::ImplDef(id).to_chalk(self.db);

        let mut result: Vec<_> = if fps.is_empty() {
            debug!("Unrestricted search for {:?} impls...", trait_);
            impl_maps
                .iter()
                .flat_map(|crate_impl_defs| crate_impl_defs.for_trait(trait_).map(id_to_chalk))
                .collect()
        } else {
            impl_maps
                .iter()
                .flat_map(|crate_impl_defs| {
                    fps.iter().flat_map(move |fp| {
                        crate_impl_defs.for_trait_and_self_ty(trait_, *fp).map(id_to_chalk)
                    })
                })
                .collect()
        };

        let arg: Option<Ty> =
            parameters.get(1).map(|p| from_chalk(self.db, p.assert_ty_ref(&Interner).clone()));

        builtin::get_builtin_impls(self.db, self.krate, &ty, &arg, trait_, |i| {
            result.push(i.to_chalk(self.db))
        });

        debug!("impls_for_trait returned {} impls", result.len());
        result
    }
    fn impl_provided_for(&self, auto_trait_id: TraitId, struct_id: AdtId) -> bool {
        debug!("impl_provided_for {:?}, {:?}", auto_trait_id, struct_id);
        false // FIXME
    }
    fn associated_ty_value(&self, id: AssociatedTyValueId) -> Arc<AssociatedTyValue> {
        self.db.associated_ty_value(self.krate, id)
    }

    fn custom_clauses(&self) -> Vec<chalk_ir::ProgramClause<Interner>> {
        vec![]
    }
    fn local_impls_to_coherence_check(&self, _trait_id: TraitId) -> Vec<ImplId> {
        // We don't do coherence checking (yet)
        unimplemented!()
    }
    fn interner(&self) -> &Interner {
        &Interner
    }
    fn well_known_trait_id(
        &self,
        well_known_trait: rust_ir::WellKnownTrait,
    ) -> Option<chalk_ir::TraitId<Interner>> {
        let lang_attr = lang_attr_from_well_known_trait(well_known_trait);
        let trait_ = match self.db.lang_item(self.krate, lang_attr.into()) {
            Some(LangItemTarget::TraitId(trait_)) => trait_,
            _ => return None,
        };
        Some(trait_.to_chalk(self.db))
    }

    fn program_clauses_for_env(
        &self,
        environment: &chalk_ir::Environment<Interner>,
    ) -> chalk_ir::ProgramClauses<Interner> {
        self.db.program_clauses_for_chalk_env(self.krate, environment.clone())
    }

    fn opaque_ty_data(&self, id: chalk_ir::OpaqueTyId<Interner>) -> Arc<OpaqueTyDatum> {
        let interned_id = crate::db::InternedOpaqueTyId::from(id);
        let full_id = self.db.lookup_intern_impl_trait_id(interned_id);
        let (func, idx) = match full_id {
            crate::OpaqueTyId::ReturnTypeImplTrait(func, idx) => (func, idx),
        };
        let datas =
            self.db.return_type_impl_traits(func).expect("impl trait id without impl traits");
        let data = &datas.value.impl_traits[idx as usize];
        let bound = OpaqueTyDatumBound {
            bounds: make_binders(
                data.bounds
                    .value
                    .iter()
                    .cloned()
                    .filter(|b| !b.is_error())
                    .map(|b| b.to_chalk(self.db))
                    .collect(),
                1,
            ),
        };
        let num_vars = datas.num_binders;
        Arc::new(OpaqueTyDatum { opaque_ty_id: id, bound: make_binders(bound, num_vars) })
    }

    fn hidden_opaque_type(&self, _id: chalk_ir::OpaqueTyId<Interner>) -> chalk_ir::Ty<Interner> {
        // FIXME: actually provide the hidden type; it is relevant for auto traits
        Ty::Unknown.to_chalk(self.db)
    }

    fn force_impl_for(
        &self,
        _well_known: rust_ir::WellKnownTrait,
        _ty: &chalk_ir::TyData<Interner>,
    ) -> Option<bool> {
        // this method is mostly for rustc
        None
    }

    fn is_object_safe(&self, _trait_id: chalk_ir::TraitId<Interner>) -> bool {
        // FIXME: implement actual object safety
        true
    }

    fn closure_kind(
        &self,
        _closure_id: chalk_ir::ClosureId<Interner>,
        _substs: &chalk_ir::Substitution<Interner>,
    ) -> rust_ir::ClosureKind {
        // FIXME: implement closure support
        unimplemented!()
    }
    fn closure_inputs_and_output(
        &self,
        _closure_id: chalk_ir::ClosureId<Interner>,
        _substs: &chalk_ir::Substitution<Interner>,
    ) -> chalk_ir::Binders<rust_ir::FnDefInputsAndOutputDatum<Interner>> {
        // FIXME: implement closure support
        unimplemented!()
    }
    fn closure_upvars(
        &self,
        _closure_id: chalk_ir::ClosureId<Interner>,
        _substs: &chalk_ir::Substitution<Interner>,
    ) -> chalk_ir::Binders<chalk_ir::Ty<Interner>> {
        // FIXME: implement closure support
        unimplemented!()
    }
    fn closure_fn_substitution(
        &self,
        _closure_id: chalk_ir::ClosureId<Interner>,
        _substs: &chalk_ir::Substitution<Interner>,
    ) -> chalk_ir::Substitution<Interner> {
        // FIXME: implement closure support
        unimplemented!()
    }

    fn trait_name(&self, _trait_id: chalk_ir::TraitId<Interner>) -> String {
        unimplemented!()
    }
    fn adt_name(&self, _struct_id: chalk_ir::AdtId<Interner>) -> String {
        unimplemented!()
    }
    fn assoc_type_name(&self, _assoc_ty_id: chalk_ir::AssocTypeId<Interner>) -> String {
        unimplemented!()
    }
    fn opaque_type_name(&self, _opaque_ty_id: chalk_ir::OpaqueTyId<Interner>) -> String {
        unimplemented!()
    }
    fn fn_def_name(&self, _fn_def_id: chalk_ir::FnDefId<Interner>) -> String {
        unimplemented!()
    }
}

pub(crate) fn program_clauses_for_chalk_env_query(
    db: &dyn HirDatabase,
    krate: CrateId,
    environment: chalk_ir::Environment<Interner>,
) -> chalk_ir::ProgramClauses<Interner> {
    chalk_solve::program_clauses_for_env(&ChalkContext { db, krate }, &environment)
}

pub(crate) fn associated_ty_data_query(
    db: &dyn HirDatabase,
    id: AssocTypeId,
) -> Arc<AssociatedTyDatum> {
    debug!("associated_ty_data {:?}", id);
    let type_alias: TypeAliasId = from_chalk(db, id);
    let trait_ = match type_alias.lookup(db.upcast()).container {
        AssocContainerId::TraitId(t) => t,
        _ => panic!("associated type not in trait"),
    };

    // Lower bounds -- we could/should maybe move this to a separate query in `lower`
    let type_alias_data = db.type_alias_data(type_alias);
    let generic_params = generics(db.upcast(), type_alias.into());
    let bound_vars = Substs::bound_vars(&generic_params, DebruijnIndex::INNERMOST);
    let resolver = hir_def::resolver::HasResolver::resolver(type_alias, db.upcast());
    let ctx = crate::TyLoweringContext::new(db, &resolver)
        .with_type_param_mode(crate::lower::TypeParamLoweringMode::Variable);
    let self_ty = Ty::Bound(crate::BoundVar::new(crate::DebruijnIndex::INNERMOST, 0));
    let bounds = type_alias_data
        .bounds
        .iter()
        .flat_map(|bound| GenericPredicate::from_type_bound(&ctx, bound, self_ty.clone()))
        .filter_map(|pred| generic_predicate_to_inline_bound(db, &pred, &self_ty))
        .map(|bound| make_binders(bound.shifted_in(&Interner), 0))
        .collect();

    let where_clauses = convert_where_clauses(db, type_alias.into(), &bound_vars);
    let bound_data = rust_ir::AssociatedTyDatumBound { bounds, where_clauses };
    let datum = AssociatedTyDatum {
        trait_id: trait_.to_chalk(db),
        id,
        name: type_alias,
        binders: make_binders(bound_data, generic_params.len()),
    };
    Arc::new(datum)
}

pub(crate) fn trait_datum_query(
    db: &dyn HirDatabase,
    krate: CrateId,
    trait_id: TraitId,
) -> Arc<TraitDatum> {
    debug!("trait_datum {:?}", trait_id);
    let trait_: hir_def::TraitId = from_chalk(db, trait_id);
    let trait_data = db.trait_data(trait_);
    debug!("trait {:?} = {:?}", trait_id, trait_data.name);
    let generic_params = generics(db.upcast(), trait_.into());
    let bound_vars = Substs::bound_vars(&generic_params, DebruijnIndex::INNERMOST);
    let flags = rust_ir::TraitFlags {
        auto: trait_data.auto,
        upstream: trait_.lookup(db.upcast()).container.module(db.upcast()).krate != krate,
        non_enumerable: true,
        coinductive: false, // only relevant for Chalk testing
        // FIXME: set these flags correctly
        marker: false,
        fundamental: false,
    };
    let where_clauses = convert_where_clauses(db, trait_.into(), &bound_vars);
    let associated_ty_ids =
        trait_data.associated_types().map(|type_alias| type_alias.to_chalk(db)).collect();
    let trait_datum_bound = rust_ir::TraitDatumBound { where_clauses };
    let well_known =
        lang_attr(db.upcast(), trait_).and_then(|name| well_known_trait_from_lang_attr(&name));
    let trait_datum = TraitDatum {
        id: trait_id,
        binders: make_binders(trait_datum_bound, bound_vars.len()),
        flags,
        associated_ty_ids,
        well_known,
    };
    Arc::new(trait_datum)
}

fn well_known_trait_from_lang_attr(name: &str) -> Option<WellKnownTrait> {
    Some(match name {
        "sized" => WellKnownTrait::Sized,
        "copy" => WellKnownTrait::Copy,
        "clone" => WellKnownTrait::Clone,
        "drop" => WellKnownTrait::Drop,
        "fn_once" => WellKnownTrait::FnOnce,
        "fn_mut" => WellKnownTrait::FnMut,
        "fn" => WellKnownTrait::Fn,
        "unsize" => WellKnownTrait::Unsize,
        _ => return None,
    })
}

fn lang_attr_from_well_known_trait(attr: WellKnownTrait) -> &'static str {
    match attr {
        WellKnownTrait::Sized => "sized",
        WellKnownTrait::Copy => "copy",
        WellKnownTrait::Clone => "clone",
        WellKnownTrait::Drop => "drop",
        WellKnownTrait::FnOnce => "fn_once",
        WellKnownTrait::FnMut => "fn_mut",
        WellKnownTrait::Fn => "fn",
        WellKnownTrait::Unsize => "unsize",
    }
}

pub(crate) fn struct_datum_query(
    db: &dyn HirDatabase,
    krate: CrateId,
    struct_id: AdtId,
) -> Arc<StructDatum> {
    debug!("struct_datum {:?}", struct_id);
    let type_ctor: TypeCtor = from_chalk(db, TypeName::Adt(struct_id));
    debug!("struct {:?} = {:?}", struct_id, type_ctor);
    let num_params = type_ctor.num_ty_params(db);
    let upstream = type_ctor.krate(db) != Some(krate);
    let where_clauses = type_ctor
        .as_generic_def()
        .map(|generic_def| {
            let generic_params = generics(db.upcast(), generic_def);
            let bound_vars = Substs::bound_vars(&generic_params, DebruijnIndex::INNERMOST);
            convert_where_clauses(db, generic_def, &bound_vars)
        })
        .unwrap_or_else(Vec::new);
    let flags = rust_ir::AdtFlags {
        upstream,
        // FIXME set fundamental and phantom_data flags correctly
        fundamental: false,
        phantom_data: false,
    };
    // FIXME provide enum variants properly (for auto traits)
    let variant = rust_ir::AdtVariantDatum {
        fields: Vec::new(), // FIXME add fields (only relevant for auto traits),
    };
    let struct_datum_bound = rust_ir::AdtDatumBound { variants: vec![variant], where_clauses };
    let struct_datum = StructDatum {
        // FIXME set ADT kind
        kind: rust_ir::AdtKind::Struct,
        id: struct_id,
        binders: make_binders(struct_datum_bound, num_params),
        flags,
    };
    Arc::new(struct_datum)
}

pub(crate) fn impl_datum_query(
    db: &dyn HirDatabase,
    krate: CrateId,
    impl_id: ImplId,
) -> Arc<ImplDatum> {
    let _p = ra_prof::profile("impl_datum");
    debug!("impl_datum {:?}", impl_id);
    let impl_: Impl = from_chalk(db, impl_id);
    match impl_ {
        Impl::ImplDef(impl_def) => impl_def_datum(db, krate, impl_id, impl_def),
        _ => Arc::new(builtin::impl_datum(db, krate, impl_).to_chalk(db)),
    }
}

fn impl_def_datum(
    db: &dyn HirDatabase,
    krate: CrateId,
    chalk_id: ImplId,
    impl_id: hir_def::ImplId,
) -> Arc<ImplDatum> {
    let trait_ref = db
        .impl_trait(impl_id)
        // ImplIds for impls where the trait ref can't be resolved should never reach Chalk
        .expect("invalid impl passed to Chalk")
        .value;
    let impl_data = db.impl_data(impl_id);

    let generic_params = generics(db.upcast(), impl_id.into());
    let bound_vars = Substs::bound_vars(&generic_params, DebruijnIndex::INNERMOST);
    let trait_ = trait_ref.trait_;
    let impl_type = if impl_id.lookup(db.upcast()).container.module(db.upcast()).krate == krate {
        rust_ir::ImplType::Local
    } else {
        rust_ir::ImplType::External
    };
    let where_clauses = convert_where_clauses(db, impl_id.into(), &bound_vars);
    let negative = impl_data.is_negative;
    debug!(
        "impl {:?}: {}{} where {:?}",
        chalk_id,
        if negative { "!" } else { "" },
        trait_ref.display(db),
        where_clauses
    );
    let trait_ref = trait_ref.to_chalk(db);

    let polarity = if negative { rust_ir::Polarity::Negative } else { rust_ir::Polarity::Positive };

    let impl_datum_bound = rust_ir::ImplDatumBound { trait_ref, where_clauses };
    let trait_data = db.trait_data(trait_);
    let associated_ty_value_ids = impl_data
        .items
        .iter()
        .filter_map(|item| match item {
            AssocItemId::TypeAliasId(type_alias) => Some(*type_alias),
            _ => None,
        })
        .filter(|&type_alias| {
            // don't include associated types that don't exist in the trait
            let name = &db.type_alias_data(type_alias).name;
            trait_data.associated_type_by_name(name).is_some()
        })
        .map(|type_alias| AssocTyValue::TypeAlias(type_alias).to_chalk(db))
        .collect();
    debug!("impl_datum: {:?}", impl_datum_bound);
    let impl_datum = ImplDatum {
        binders: make_binders(impl_datum_bound, bound_vars.len()),
        impl_type,
        polarity,
        associated_ty_value_ids,
    };
    Arc::new(impl_datum)
}

pub(crate) fn associated_ty_value_query(
    db: &dyn HirDatabase,
    krate: CrateId,
    id: AssociatedTyValueId,
) -> Arc<AssociatedTyValue> {
    let data: AssocTyValue = from_chalk(db, id);
    match data {
        AssocTyValue::TypeAlias(type_alias) => {
            type_alias_associated_ty_value(db, krate, type_alias)
        }
        _ => Arc::new(builtin::associated_ty_value(db, krate, data).to_chalk(db)),
    }
}

fn type_alias_associated_ty_value(
    db: &dyn HirDatabase,
    _krate: CrateId,
    type_alias: TypeAliasId,
) -> Arc<AssociatedTyValue> {
    let type_alias_data = db.type_alias_data(type_alias);
    let impl_id = match type_alias.lookup(db.upcast()).container {
        AssocContainerId::ImplId(it) => it,
        _ => panic!("assoc ty value should be in impl"),
    };

    let trait_ref = db.impl_trait(impl_id).expect("assoc ty value should not exist").value; // we don't return any assoc ty values if the impl'd trait can't be resolved

    let assoc_ty = db
        .trait_data(trait_ref.trait_)
        .associated_type_by_name(&type_alias_data.name)
        .expect("assoc ty value should not exist"); // validated when building the impl data as well
    let ty = db.ty(type_alias.into());
    let value_bound = rust_ir::AssociatedTyValueBound { ty: ty.value.to_chalk(db) };
    let value = rust_ir::AssociatedTyValue {
        impl_id: Impl::ImplDef(impl_id).to_chalk(db),
        associated_ty_id: assoc_ty.to_chalk(db),
        value: make_binders(value_bound, ty.num_binders),
    };
    Arc::new(value)
}

pub(crate) fn fn_def_datum_query(
    db: &dyn HirDatabase,
    _krate: CrateId,
    fn_def_id: FnDefId,
) -> Arc<FnDefDatum> {
    let callable_def: CallableDef = from_chalk(db, fn_def_id);
    let generic_params = generics(db.upcast(), callable_def.into());
    let sig = db.callable_item_signature(callable_def);
    let bound_vars = Substs::bound_vars(&generic_params, DebruijnIndex::INNERMOST);
    let where_clauses = convert_where_clauses(db, callable_def.into(), &bound_vars);
    let bound = rust_ir::FnDefDatumBound {
        // Note: Chalk doesn't actually use this information yet as far as I am aware, but we provide it anyway
        inputs_and_output: make_binders(
            rust_ir::FnDefInputsAndOutputDatum {
                argument_types: sig
                    .value
                    .params()
                    .iter()
                    .map(|ty| ty.clone().to_chalk(db))
                    .collect(),
                return_type: sig.value.ret().clone().to_chalk(db),
            }
            .shifted_in(&Interner),
            0,
        ),
        where_clauses,
    };
    let datum =
        FnDefDatum { id: fn_def_id, binders: make_binders(bound, sig.num_binders), abi: () };
    Arc::new(datum)
}

impl From<AdtId> for crate::TypeCtorId {
    fn from(struct_id: AdtId) -> Self {
        struct_id.0
    }
}

impl From<crate::TypeCtorId> for AdtId {
    fn from(type_ctor_id: crate::TypeCtorId) -> Self {
        chalk_ir::AdtId(type_ctor_id)
    }
}

impl From<FnDefId> for crate::CallableDefId {
    fn from(fn_def_id: FnDefId) -> Self {
        InternKey::from_intern_id(fn_def_id.0)
    }
}

impl From<crate::CallableDefId> for FnDefId {
    fn from(callable_def_id: crate::CallableDefId) -> Self {
        chalk_ir::FnDefId(callable_def_id.as_intern_id())
    }
}

impl From<ImplId> for crate::traits::GlobalImplId {
    fn from(impl_id: ImplId) -> Self {
        InternKey::from_intern_id(impl_id.0)
    }
}

impl From<crate::traits::GlobalImplId> for ImplId {
    fn from(impl_id: crate::traits::GlobalImplId) -> Self {
        chalk_ir::ImplId(impl_id.as_intern_id())
    }
}

impl From<OpaqueTyId> for crate::db::InternedOpaqueTyId {
    fn from(id: OpaqueTyId) -> Self {
        InternKey::from_intern_id(id.0)
    }
}

impl From<crate::db::InternedOpaqueTyId> for OpaqueTyId {
    fn from(id: crate::db::InternedOpaqueTyId) -> Self {
        chalk_ir::OpaqueTyId(id.as_intern_id())
    }
}

impl From<rust_ir::AssociatedTyValueId<Interner>> for crate::traits::AssocTyValueId {
    fn from(id: rust_ir::AssociatedTyValueId<Interner>) -> Self {
        Self::from_intern_id(id.0)
    }
}

impl From<crate::traits::AssocTyValueId> for rust_ir::AssociatedTyValueId<Interner> {
    fn from(assoc_ty_value_id: crate::traits::AssocTyValueId) -> Self {
        rust_ir::AssociatedTyValueId(assoc_ty_value_id.as_intern_id())
    }
}
