//! Collects trait impls for each item in the crate. For example, if a crate
//! defines a struct that implements a trait, this pass will note that the
//! struct implements that trait.
use super::Pass;
use crate::clean::*;
use crate::core::DocContext;
use crate::visit::DocVisitor;

use rustc_data_structures::fx::{FxHashMap, FxHashSet};
use rustc_hir::def_id::DefId;
use rustc_middle::ty::DefIdTree;
use rustc_span::symbol::sym;

crate const COLLECT_TRAIT_IMPLS: Pass = Pass {
    name: "collect-trait-impls",
    run: collect_trait_impls,
    description: "retrieves trait impls for items in the crate",
};

crate fn collect_trait_impls(mut krate: Crate, cx: &mut DocContext<'_>) -> Crate {
    let synth_impls = cx.sess().time("collect_synthetic_impls", || {
        let mut synth = SyntheticImplCollector { cx, impls: Vec::new() };
        synth.visit_crate(&krate);
        synth.impls
    });

    let prims: FxHashSet<PrimitiveType> = krate.primitives.iter().map(|p| p.1).collect();

    let crate_items = {
        let mut coll = ItemCollector::new();
        cx.sess().time("collect_items_for_trait_impls", || coll.visit_crate(&krate));
        coll.items
    };

    let mut new_items = Vec::new();

    // External trait impls.
    cx.with_all_trait_impls(|cx, all_trait_impls| {
        let _prof_timer = cx.tcx.sess.prof.generic_activity("build_extern_trait_impls");
        for &impl_def_id in all_trait_impls.iter().skip_while(|def_id| def_id.is_local()) {
            inline::build_impl(cx, None, impl_def_id, None, &mut new_items);
        }
    });

    // Also try to inline primitive impls from other crates.
    cx.tcx.sess.prof.generic_activity("build_primitive_trait_impls").run(|| {
        for &def_id in PrimitiveType::all_impls(cx.tcx).values().flatten() {
            if !def_id.is_local() {
                inline::build_impl(cx, None, def_id, None, &mut new_items);

                // FIXME(eddyb) is this `doc(hidden)` check needed?
                if !cx.tcx.is_doc_hidden(def_id) {
                    let impls = get_auto_trait_and_blanket_impls(cx, def_id);
                    new_items.extend(impls.filter(|i| cx.inlined.insert(i.def_id)));
                }
            }
        }
    });

    let mut cleaner = BadImplStripper { prims, items: crate_items };
    let mut type_did_to_deref_target: FxHashMap<DefId, &Type> = FxHashMap::default();

    // Follow all `Deref` targets of included items and recursively add them as valid
    fn add_deref_target(
        cx: &DocContext<'_>,
        map: &FxHashMap<DefId, &Type>,
        cleaner: &mut BadImplStripper,
        type_did: DefId,
    ) {
        if let Some(target) = map.get(&type_did) {
            debug!("add_deref_target: type {:?}, target {:?}", type_did, target);
            if let Some(target_prim) = target.primitive_type() {
                cleaner.prims.insert(target_prim);
            } else if let Some(target_did) = target.def_id(&cx.cache) {
                // `impl Deref<Target = S> for S`
                if target_did == type_did {
                    // Avoid infinite cycles
                    return;
                }
                cleaner.items.insert(target_did.into());
                add_deref_target(cx, map, cleaner, target_did);
            }
        }
    }

    // scan through included items ahead of time to splice in Deref targets to the "valid" sets
    for it in &new_items {
        if let ImplItem(Impl { ref for_, ref trait_, ref items, .. }) = *it.kind {
            if trait_.as_ref().map(|t| t.def_id()) == cx.tcx.lang_items().deref_trait()
                && cleaner.keep_impl(for_, true)
            {
                let target = items
                    .iter()
                    .find_map(|item| match *item.kind {
                        TypedefItem(ref t, true) => Some(&t.type_),
                        _ => None,
                    })
                    .expect("Deref impl without Target type");

                if let Some(prim) = target.primitive_type() {
                    cleaner.prims.insert(prim);
                } else if let Some(did) = target.def_id(&cx.cache) {
                    cleaner.items.insert(did.into());
                }
                if let Some(for_did) = for_.def_id_no_primitives() {
                    if type_did_to_deref_target.insert(for_did, target).is_none() {
                        // Since only the `DefId` portion of the `Type` instances is known to be same for both the
                        // `Deref` target type and the impl for type positions, this map of types is keyed by
                        // `DefId` and for convenience uses a special cleaner that accepts `DefId`s directly.
                        if cleaner.keep_impl_with_def_id(for_did.into()) {
                            add_deref_target(cx, &type_did_to_deref_target, &mut cleaner, for_did);
                        }
                    }
                }
            }
        }
    }

    new_items.retain(|it| {
        if let ImplItem(Impl { ref for_, ref trait_, ref kind, .. }) = *it.kind {
            cleaner.keep_impl(
                for_,
                trait_.as_ref().map(|t| t.def_id()) == cx.tcx.lang_items().deref_trait(),
            ) || trait_.as_ref().map_or(false, |t| cleaner.keep_impl_with_def_id(t.def_id().into()))
                || kind.is_blanket()
        } else {
            true
        }
    });

    // Local trait impls.
    cx.with_all_trait_impls(|cx, all_trait_impls| {
        let _prof_timer = cx.tcx.sess.prof.generic_activity("build_local_trait_impls");
        let mut attr_buf = Vec::new();
        for &impl_def_id in all_trait_impls.iter().take_while(|def_id| def_id.is_local()) {
            let mut parent = cx.tcx.parent(impl_def_id);
            while let Some(did) = parent {
                attr_buf.extend(
                    cx.tcx
                        .get_attrs(did)
                        .iter()
                        .filter(|attr| attr.has_name(sym::doc))
                        .filter(|attr| {
                            if let Some([attr]) = attr.meta_item_list().as_deref() {
                                attr.has_name(sym::cfg)
                            } else {
                                false
                            }
                        })
                        .cloned(),
                );
                parent = cx.tcx.parent(did);
            }
            inline::build_impl(cx, None, impl_def_id, Some(&attr_buf), &mut new_items);
            attr_buf.clear();
        }
    });

    if let ModuleItem(Module { items, .. }) = &mut *krate.module.kind {
        items.extend(synth_impls);
        items.extend(new_items);
    } else {
        panic!("collect-trait-impls can't run");
    };

    krate
}

struct SyntheticImplCollector<'a, 'tcx> {
    cx: &'a mut DocContext<'tcx>,
    impls: Vec<Item>,
}

impl<'a, 'tcx> DocVisitor for SyntheticImplCollector<'a, 'tcx> {
    fn visit_item(&mut self, i: &Item) {
        if i.is_struct() || i.is_enum() || i.is_union() {
            // FIXME(eddyb) is this `doc(hidden)` check needed?
            if !self.cx.tcx.is_doc_hidden(i.def_id.expect_def_id()) {
                self.impls
                    .extend(get_auto_trait_and_blanket_impls(self.cx, i.def_id.expect_def_id()));
            }
        }

        self.visit_item_recur(i)
    }
}

#[derive(Default)]
struct ItemCollector {
    items: FxHashSet<ItemId>,
}

impl ItemCollector {
    fn new() -> Self {
        Self::default()
    }
}

impl DocVisitor for ItemCollector {
    fn visit_item(&mut self, i: &Item) {
        self.items.insert(i.def_id);

        self.visit_item_recur(i)
    }
}

struct BadImplStripper {
    prims: FxHashSet<PrimitiveType>,
    items: FxHashSet<ItemId>,
}

impl BadImplStripper {
    fn keep_impl(&self, ty: &Type, is_deref: bool) -> bool {
        if let Generic(_) = ty {
            // keep impls made on generics
            true
        } else if let Some(prim) = ty.primitive_type() {
            self.prims.contains(&prim)
        } else if let Some(did) = ty.def_id_no_primitives() {
            is_deref || self.keep_impl_with_def_id(did.into())
        } else {
            false
        }
    }

    fn keep_impl_with_def_id(&self, did: ItemId) -> bool {
        self.items.contains(&did)
    }
}
