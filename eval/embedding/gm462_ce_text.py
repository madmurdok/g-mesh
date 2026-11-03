#!/usr/bin/env python3
"""GM-462: the enriched cross-encoder input text.

`build` puts up to four fields around today's CE text (`base`, the
structured text that Rust `text_to_embed` produces):

    <E>\\n<I>\\n<P>\\n<base>\\n<B>

  E  enclosing: split words of the node's native kind, then of the
     qualified-name prefix (the qualified name minus its trailing name);
  I  identifiers: split words of the name, then of each distinct signature
     identifier (keywords and the name itself dropped), cut at 48 tokens;
  P  path: split words of the file path;
  B  body: source lines after the declaration's first line (Function and
     Type only), within a token budget (96 by default).

An empty field is left out together with its newline, so with every switch
off `build` returns `base` byte for byte. The short header fields go first:
the pair tokenizers truncate longest-first, so an over-long pair loses the
body and then the end of `base`, never the header.

`tok` is a callable that returns the token count of a string without special
tokens (`token_counter` makes one from a tokenizers.Tokenizer). The rules are
specified in docs/design/GM-462-richer-reranker-input.md, section 2.
"""
import re

from gm423_token_lengths import form_text, text_to_embed

SWITCHES = frozenset("EIPB")
I_CAP = 48
BODY_BUDGET = 96
LINE_CHARS = 200
BODY_KINDS = ("Function", "Type")

_WORD = re.compile(r"[A-Z]+(?=[A-Z][a-z])|[A-Z]?[a-z]+|[A-Z]+|[0-9]+")
_IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
_PUNCT_ONLY = re.compile(r"^[\s{}()\[\];,]*$")
_DOCSTRING_OPEN = re.compile(r"^[rRbBuUfF]{0,2}(\"\"\"|''')")

# Copied from core/src/embedding/text.rs `SIGNATURE_KEYWORDS`; compared lowercased.
SIGNATURE_KEYWORDS = frozenset([
    "pub", "crate", "super", "in", "fn", "mod", "const", "static", "struct", "enum", "func", "def", "async",
    "let", "export", "self", "mut", "var", "type", "impl", "unsafe", "extern",
])


def token_counter(tokenizer):
    """A `tok` callable over a tokenizers.Tokenizer. The tokenizer must not
    truncate, or counts above its limit come back capped."""
    return lambda s: len(tokenizer.encode(s, add_special_tokens=False).ids) if s else 0


def split_words(s):
    """`HTTPServer` -> `http server`, `safeInt8` -> `safe int 8`, `send_quit` -> `send quit`."""
    return " ".join(w.lower() for w in _WORD.findall(s or ""))


def _join(*parts):
    return " ".join(p for p in parts if p)


def enclosing(kind, native_kind, name, qualified_name):
    """Kind words, then the words of the qualified name minus its trailing `name`
    (the whole qualified name when it does not end in `name`)."""
    qn = qualified_name or ""
    prefix = qn[:-len(name)] if name and qn.endswith(name) else qn
    return _join(split_words(native_kind or kind), split_words(prefix))


def identifiers(name, signature, tok, cap=I_CAP):
    """Words of `name`, then of each distinct signature identifier in source
    order (keywords and `name` dropped), cut at `cap` tokens on a word boundary."""
    seen, idents = {name}, []
    for m in _IDENT.finditer(signature or ""):
        w = m.group(0)
        if w.lower() not in SIGNATURE_KEYWORDS and w not in seen:
            seen.add(w)
            idents.append(w)
    words = _join(split_words(name), *(split_words(w) for w in idents)).split()
    out, n = [], 0
    for w in words:
        n += tok(w)
        if n > cap:
            break
        out.append(w)
    return " ".join(out)


def _skip_docstring(lines):
    """Index of the first line after a leading Python docstring (0 when there is none)."""
    i = 0
    while i < len(lines) and not lines[i].strip():
        i += 1
    if i == len(lines):
        return 0
    head = lines[i].strip()
    m = _DOCSTRING_OPEN.match(head)
    if not m:
        return 0
    q = m.group(1)
    if q in head[m.end():]:
        return i + 1
    i += 1
    while i < len(lines) and q not in lines[i]:
        i += 1
    return i + 1


def body_lines(lines, start, end, language):
    """Source lines `start+1 ..= end` (0-based, the declaration's first line
    skipped), a leading Python docstring skipped, lines holding only
    `{}()[];,` and whitespace dropped, whitespace collapsed, each line cut at
    LINE_CHARS characters."""
    ls = lines[start + 1:end + 1]
    if language == "python":
        ls = ls[_skip_docstring(ls):]
    return [" ".join(l.split())[:LINE_CHARS] for l in ls if not _PUNCT_ONLY.match(l)]


def body(lines, start, end, kind, language, tok, budget=BODY_BUDGET):
    """Whole body lines, joined by newlines, while the field stays within
    `budget` tokens; stops at the first line that does not fit. Empty for
    kinds other than Function and Type, or without source lines."""
    if kind not in BODY_KINDS or not lines:
        return ""
    out, n = [], 0
    for l in body_lines(lines, start, end, language):
        n += tok(l)
        if n > budget:
            break
        out.append(l)
    return "\n".join(out)


def base_text(row):
    """Today's CE text: the structured text (Rust `text_to_embed`)."""
    doc, sig = row.get("docComment"), row.get("signature")
    return form_text("structured", doc, sig, text_to_embed(doc, sig)) or ""


def build(row, src_lines, switches, tok, body_budget=BODY_BUDGET):
    """The CE text of one node. `row` holds the eval DB columns (kind,
    nativeKind, name, qualifiedName, filePath, startLine, endLine, signature,
    docComment, language); `src_lines` is the node's source file split on
    newlines, or None; `switches` is a subset of SWITCHES."""
    bad = set(switches) - SWITCHES
    if bad:
        raise ValueError(f"unknown switches {sorted(bad)}")
    name = row.get("name") or ""
    fields = [
        enclosing(row.get("kind"), row.get("nativeKind"), name, row.get("qualifiedName")) if "E" in switches else "",
        identifiers(name, row.get("signature"), tok) if "I" in switches else "",
        split_words(row.get("filePath")) if "P" in switches else "",
        base_text(row),
        body(src_lines, row.get("startLine"), row.get("endLine"), row.get("kind"), row.get("language"), tok,
             body_budget) if "B" in switches else "",
    ]
    return "\n".join(f for f in fields if f)
