//! The questions the structural pass leaves for a semantic tier, as SDK open
//! sites (section 1.4 of `docs/architecture/gm-324-typescript-rust-port.md`).
//! None of them reaches the wire: they feed an engine's `SdkIndex`.
//!
//! - **`OverloadCall`.** A call whose `CALLS` edge targets a node of this file
//!   with two or more declarations, or a `pending_symbol` placeholder (whose
//!   declarations live in a file this walk does not read). It refines that
//!   edge, which it names in `replaces` (ADR 0024,
//!   `docs/adr/0024-semantic-tier-refines-by-binding-a-declaration.md`). One
//!   site per call, not per edge: two calls of one overload set from one
//!   function share an edge and may bind different declarations.
//! - **`Reference`.** `ns.member` and `ns.member()` on `import * as ns` of a
//!   project file, unless a local or a declaration of this file shadows `ns`.
//!   No edge exists for it; `edge_kind` is `CALLS` for a call made inside a
//!   function, else `REFERENCES`.
//! - **`Reference` with `replaces`.** The first use behind each `CALLS`,
//!   `REFERENCES` or `SUPERTYPE_OF` edge onto a `pending_symbol` placeholder: an imported name,
//!   which may reach its declaration only through a re-export or as a
//!   `default`. `replaces` names that edge, and the bridge asks only where
//!   the linker cannot settle the placeholder itself (design note
//!   `docs/architecture/gm-325-typescript-lsp-semantics.md`, section 4.3). A
//!   call may carry both this and an `OverloadCall` site; the bridge keeps
//!   one question.
//! - **`ReceiverCall`.** `obj.m()` that reaches no member declared here, any
//!   call through a longer receiver (`a.b.m()`, `f().m()`), and `this.m()` /
//!   `super.m()` that binds no member declared here. No edge exists for it.
//!   The builder folds these into `untypedCalls` ([`crate::extractor::emit`]).
//!
//! Every site points at the name token, in wire columns, and has no
//! `from_container`: this plugin's targets are files, not containers.

use g_mesh_plugin_sdk::ids::edge_id;
use g_mesh_plugin_sdk::wire::EdgeKind;
use g_mesh_plugin_sdk::{OpenSite, OpenSiteKind};
use tree_sitter::Node;

use crate::extractor::decls::Declarer;
use crate::extractor::keys::PENDING_SYMBOL_NATIVE_KIND;
use crate::extractor::model::{CallSite, PendingMemberAccess};
use crate::extractor::scope::{is_locally_bound, Scope};

impl<'a, 's, 't> Declarer<'a, 's, 't> {
    /// Remembers where a call that produced the `CALLS` edge `from -> to` was
    /// written. Whether it is a question is settled once the declaration
    /// lists are ([`record_overload_call_sites`](Self::record_overload_call_sites)).
    pub(super) fn record_call_site(&mut self, from_id: &str, to_id: &str, at: Node<'t>) {
        if !self.model.has_edge(from_id, EdgeKind::Calls, to_id) {
            return;
        }
        let start = at.start_position();
        self.uses.call_sites.push(CallSite {
            from_id: from_id.to_string(),
            to_id: to_id.to_string(),
            name: self.text(at).to_string(),
            position: self.columns.at(start.row, start.column),
        });
    }

    /// An `OverloadCall` site for every recorded call whose target is an
    /// overload set of this file or a `pending_symbol` placeholder. Runs after
    /// the declaration lists are settled: a later overload may be written
    /// below the call.
    pub(super) fn record_overload_call_sites(&mut self) {
        for site in std::mem::take(&mut self.uses.call_sites) {
            let Some(target) = self.model.node_by_id(&site.to_id) else { continue };
            let kept = target.native_kind.as_deref() == Some(PENDING_SYMBOL_NATIVE_KIND)
                || target.declarations.is_some();
            if !kept {
                continue;
            }
            let replaces = edge_id(&site.from_id, EdgeKind::Calls, &site.to_id, None);
            self.model.add_open_site(OpenSite {
                from_id: site.from_id,
                position: site.position,
                name: site.name,
                kind: OpenSiteKind::OverloadCall,
                edge_kind: EdgeKind::Calls,
                from_container: None,
                replaces: Some(replaces),
            });
        }
    }

    /// A `Reference` site with `replaces` for the use written at `at` that
    /// produced the edge `from_id -edge_kind-> to_id`, when `to_id` is a
    /// `pending_symbol` placeholder and the edge has no such site yet.
    pub(super) fn record_placeholder_use_site(
        &mut self,
        from_id: &str,
        edge_kind: EdgeKind,
        to_id: &str,
        at: Node<'t>,
    ) {
        if !self.model.has_edge(from_id, edge_kind, to_id) {
            return;
        }
        let Some(target) = self.model.node_by_id(to_id) else { return };
        if target.native_kind.as_deref() != Some(PENDING_SYMBOL_NATIVE_KIND) {
            return;
        }
        let replaces = edge_id(from_id, edge_kind, to_id, None);
        if !self.uses.hop_edges.insert(replaces.clone()) {
            return;
        }
        let start = at.start_position();
        self.model.add_open_site(OpenSite {
            from_id: from_id.to_string(),
            position: self.columns.at(start.row, start.column),
            name: self.text(at).to_string(),
            kind: OpenSiteKind::Reference,
            edge_kind,
            from_container: None,
            replaces: Some(replaces),
        });
    }

    /// A `ReceiverCall` site for the call of `property`: from the enclosing
    /// caller, or at module top level from the enclosing symbol.
    pub(super) fn record_receiver_call(&mut self, property: Node<'t>, scope: &Scope) {
        let from_id = scope.enclosing_caller_id.clone().unwrap_or_else(|| scope.enclosing_symbol_id.clone());
        self.add_site(property, from_id, OpenSiteKind::ReceiverCall, EdgeKind::Calls);
    }

    /// Whether `name`, written in `scope`, is a namespace import of a project
    /// file: bound by `import * as name`, and shadowed by neither a local nor
    /// a declaration of this file.
    pub(super) fn is_namespace_receiver(&self, name: &str, scope: &Scope) -> bool {
        self.namespace_binding(name).is_some()
            && !is_locally_bound(name, scope.locals.as_ref())
            && self.model.lookup_by_name(name, &scope.namespace_prefix, None).is_none()
    }

    /// A `Reference` site for `ns.member` when `ns` is a namespace import. A
    /// call made from a function asks for a `CALLS` edge from it; anything
    /// else (a read, a call at module top level) a `REFERENCES` edge from the
    /// enclosing symbol.
    pub(super) fn collect_namespace_member_use(&mut self, access: &PendingMemberAccess<'t>) {
        if !self.is_namespace_receiver(&access.object_name, &access.scope) {
            return;
        }
        let (from_id, edge_kind) = match &access.scope.enclosing_caller_id {
            Some(caller) if access.is_call => (caller.clone(), EdgeKind::Calls),
            _ => (access.scope.enclosing_symbol_id.clone(), EdgeKind::References),
        };
        self.add_site(access.at, from_id, OpenSiteKind::Reference, edge_kind);
    }

    fn add_site(&mut self, at: Node<'t>, from_id: String, kind: OpenSiteKind, edge_kind: EdgeKind) {
        let start = at.start_position();
        self.model.add_open_site(OpenSite {
            from_id,
            position: self.columns.at(start.row, start.column),
            name: self.text(at).to_string(),
            kind,
            edge_kind,
            from_container: None,
            replaces: None,
        });
    }
}
