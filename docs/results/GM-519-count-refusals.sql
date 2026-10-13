-- GM-519: count the usage edges core's linker refused, per language, tier and
-- reason, read straight off a finished project index (no instrumentation).
--
-- Run against a daemon/reindex index after its semantic pass completed:
--   sqlite3 -header -column ~/.g-mesh/projects/<id>/index.db \
--     < docs/results/GM-519-count-refusals.sql
--
-- What it relies on (core/src/graph/symbol_links.rs, `link`): a placeholder
-- the linker cannot settle keeps its usage edges on itself with
-- `resolved = 0` and `linkedFrom IS NULL`; a settled one moves them onto the
-- declaration. A semantic answer is always addressed by `qualifiedName` at
-- the declaration's own container (or file) - plugins/sdk/src/lsp/bridge.rs
-- `address_of`, and plugins/go/semantic.go's placeholders - so for a
-- semantic edge "a declaration exists at exactly that address, yet the edge
-- is still on its placeholder" means the linker refused an answer the
-- engine gave.
--
-- `visible_rule` below restates `Resolver::visible` + `Resolver::sees`:
--   public                     -> visible
--   file                       -> only when the address is a *container* scope
--                                 and the declaration is in the requester's file
--   container(c)               -> requester has a container, same language, and
--                                 c is that container or on its parent chain
-- Outcome per unresolved edge:
--   refused_visibility_file      every declaration at the address is invisible, and
--                                at least one is `file`-visible (the GM-325/S11 rule)
--   refused_visibility_container every declaration is invisible, all `container(..)`
--   refused_other                a visible declaration exists but the edge stayed:
--                                ambiguity (several) or wrong kind for the edge
--   no_declaration_at_address    nothing declared at the address (not a refusal:
--                                stale, out of index, or a re-export head walk)
-- Only `keyKind = 'qualifiedName'` placeholders are classified; `name`-keyed
-- (structural import) placeholders are counted in `structural_name_keyed`
-- unclassified, since their walk follows re-exports this query does not.

WITH RECURSIVE
unresolved AS (
    SELECT e.id AS edge_id, e.kind AS edge_kind, e.source, p.id AS ph_id,
           p.language, t.scopeKind, t.scope, t.keyKind, t.key,
           NULLIF(t.fromContainer, '') AS from_container, t.fromFile
    FROM edges e
    JOIN nodes p ON p.id = e.toId AND p.nativeKind = 'pending_symbol'
    JOIN placeholder_targets t ON t.nodeId = p.id
    WHERE e.resolved = 0 AND e.linkedFrom IS NULL
      AND e.kind IN ('CALLS', 'REFERENCES', 'SUPERTYPE_OF')
),
-- Every (language, requester container, ancestor-or-self) triple.
chain(language, start, key) AS (
    SELECT DISTINCT language, from_container, from_container
    FROM unresolved WHERE from_container IS NOT NULL
    UNION
    SELECT c.language, chain.start, c.parentKey
    FROM chain JOIN containers c ON c.language = chain.language AND c.key = chain.key
    WHERE c.parentKey IS NOT NULL
),
candidates AS (
    SELECT u.edge_id, d.id AS decl_id, d.visibility,
           CASE
             WHEN d.visibility = 'public' THEN 1
             WHEN d.visibility = 'file' THEN (u.scopeKind = 'container' AND d.filePath = u.fromFile)
             WHEN d.visibility = 'container' THEN (
               d.language = u.language AND u.from_container IS NOT NULL AND EXISTS (
                 SELECT 1 FROM chain ch WHERE ch.language = u.language
                   AND ch.start = u.from_container AND ch.key = d.visibilityContainer))
             ELSE 0
           END AS visible_rule
    FROM unresolved u
    JOIN nodes d ON d.qualifiedName = u.key
     AND ((u.scopeKind = 'file' AND d.filePath = u.scope)
       OR (u.scopeKind = 'container' AND d.language = u.language AND d.container = u.scope))
     AND (d.nativeKind IS NULL OR d.nativeKind NOT IN
          ('pending_symbol', 'reexport', 'resolved_module', 'external_module', 'container'))
     AND d.kind != 'File'
    WHERE u.keyKind = 'qualifiedName'
),
classified AS (
    SELECT u.edge_id, u.language, u.source, u.edge_kind, u.keyKind,
      CASE
        WHEN u.keyKind = 'name' THEN 'structural_name_keyed'
        WHEN NOT EXISTS (SELECT 1 FROM candidates c WHERE c.edge_id = u.edge_id)
          THEN 'no_declaration_at_address'
        WHEN EXISTS (SELECT 1 FROM candidates c WHERE c.edge_id = u.edge_id AND c.visible_rule)
          THEN 'refused_other'
        WHEN EXISTS (SELECT 1 FROM candidates c WHERE c.edge_id = u.edge_id AND c.visibility = 'file')
          THEN 'refused_visibility_file'
        ELSE 'refused_visibility_container'
      END AS outcome
    FROM unresolved u
)
SELECT language, source, outcome, COUNT(*) AS edges
FROM classified
GROUP BY language, source, outcome
UNION ALL
-- Denominator: every semantic usage edge the linker did settle.
SELECT p.language, e.source, 'linked' AS outcome, COUNT(*)
FROM edges e JOIN nodes p ON p.id = e.linkedFrom
WHERE e.resolved = 1 AND e.kind IN ('CALLS', 'REFERENCES', 'SUPERTYPE_OF')
GROUP BY p.language, e.source
ORDER BY 1, 2, 3;

-- Second count: option A's structural side effect (fixture t01/t02). A
-- `name`-keyed placeholder (a structural import: `import { handler }`) is
-- meant to land on a top-level declaration; one the linker linked onto a
-- *type member* instead (`Router#handler`) is a suspect wrong link. A member
-- is a node whose qualifiedName is a Type's qualifiedName in the same file,
-- plus `#`/`.`/`::`, plus the key. Every row this prints is worth reading by
-- hand; the count is an upper bound on wrong links of this shape.
SELECT p.language, e.kind AS edge_kind, COUNT(*) AS name_keyed_links_onto_a_member
FROM edges e
JOIN nodes p ON p.id = e.linkedFrom
JOIN placeholder_targets t ON t.nodeId = p.id AND t.keyKind = 'name'
JOIN nodes d ON d.id = e.toId
WHERE e.resolved = 1
  AND EXISTS (
    SELECT 1 FROM nodes ty
    WHERE ty.kind = 'Type' AND ty.filePath = d.filePath
      AND d.qualifiedName IN (ty.qualifiedName || '#' || t.key,
                              ty.qualifiedName || '.' || t.key,
                              ty.qualifiedName || '::' || t.key))
GROUP BY p.language, e.kind
ORDER BY 1, 2;
