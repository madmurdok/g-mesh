"""Fake quantisation of the RR cross-encoder that emulates export.quantize("l6")
(docs/design/GM-734-rr-qat.md, sections 0.1 and 1.1).

What the l6 recipe quantises, and what `wrap` emulates in torch:
  - the 38 Linears (36 encoder, pooler dense, classifier): weights by the closed form
    (fq_weight, per output channel), input activations by DynamicQuantizeLinear (fq_act);
  - the 12 activation x activation MatMuls of self-attention: Q, K, the softmax
    probabilities and V, each through fq_act;
  - the word, position and token-type tables: closed form per tensor.
LayerNorm, GELU, softmax, biases and tanh stay fp32.

fq_act takes one range per pair: padded positions are zeroed before the min/max, and
since the range always contains 0 it equals the range of the pair scored alone (chunk 1,
as quint8_check runs ORT). Values are equal only up to fp32 rounding, and a value that
rounding moves across a quantisation boundary changes one level, so logits of the
emulation, of a padded batch and of ORT differ by such level flips (not bounded by fp32
epsilon). The attention mask reaches the wrappers through a forward
pre-hook on the wrapped model (or mask_context for a direct call).

Forward values are the quantised ones exactly (`deq + (x - x.detach())`); gradients pass
straight through (STE). The fake-quant path is fp32 only: under bf16 autocast x / s
would round before the 255-level grid.

The wrappers keep the original Parameter objects under the original names, so
trainable_state / load_trainable / params_sha256 of a wrapped model equal the plain
model's.
"""

import contextlib
import math
import types

import torch
import torch.nn.functional as F

CLOSED_ZP = 127.0
CLOSED_QMAX = 127.0
ACT_LEVELS = 255.0

LINEARS = ("attention.self.query", "attention.self.key", "attention.self.value", "attention.output.dense",
           "intermediate.dense", "output.dense")
TABLES = ("bert.embeddings.word_embeddings", "bert.embeddings.position_embeddings",
          "bert.embeddings.token_type_embeddings")


def targets(model):
    """(linear module paths, table module paths, self-attention module paths) of a BERT
    sequence classifier: the 38 Linears and 3 tables export.quantize("l6") quantises."""
    n = model.config.num_hidden_layers
    lin = [f"bert.encoder.layer.{i}.{s}" for i in range(n) for s in LINEARS] + ["bert.pooler.dense", "classifier"]
    attn = [f"bert.encoder.layer.{i}.attention.self" for i in range(n)]
    return lin, list(TABLES), attn


def fq_weight(w, axis):
    """The closed form of export.closed_form, dequantised: s = maxabs / 127 per channel
    along `axis` (None = per tensor), q = clamp(round(w / s) + 127, 0, 255),
    value (q - 127) s; gradient identity."""
    with torch.no_grad():
        if axis is None:
            maxabs = w.abs().amax()
        else:
            dims = tuple(d for d in range(w.dim()) if d != axis)
            maxabs = w.abs().amax(dim=dims, keepdim=True)
        s = maxabs / CLOSED_QMAX
        s = torch.where(s == 0, torch.ones_like(s), s)
        q = torch.clamp(torch.round(w / s) + CLOSED_ZP, 0.0, 255.0)
        deq = (q - CLOSED_ZP) * s
    return deq + (w - w.detach())


def fq_act(x, mask=None):
    """ORT DynamicQuantizeLinear + dequantisation, one range per batch row: range
    [min(0, x_min), max(0, x_max)] over all non-batch dims of the row with the positions
    where `mask` (broadcastable to x) is 0 set to 0; s = range / 255,
    zp = clamp(round(-min / s), 0, 255), q = clamp(round(x / s) + zp, 0, 255),
    value (q - zp) s; gradient identity."""
    with torch.no_grad():
        xr = x if mask is None else x * mask.to(x.dtype)
        flat = xr.reshape(x.shape[0], -1)
        lo = flat.amin(1).clamp(max=0.0)
        hi = flat.amax(1).clamp(min=0.0)
        shape = (x.shape[0],) + (1,) * (x.dim() - 1)
        lo, hi = lo.reshape(shape), hi.reshape(shape)
        s = (hi - lo) / ACT_LEVELS
        s = torch.where(s == 0, torch.ones_like(s), s)
        zp = torch.clamp(torch.round(-lo / s), 0.0, 255.0)
        q = torch.clamp(torch.round(x / s) + zp, 0.0, 255.0)
        deq = (q - zp) * s
    return deq + (x - x.detach())


class _State:
    """Shared by every wrapper of one model: on/off and the current [B, S] mask."""

    def __init__(self):
        self.on = True
        self.mask = None

    def seq_mask(self, x):
        """The mask as [B, S, 1] for an activation x of shape [B, S, H], else None."""
        m = self.mask
        if m is None or x.dim() != 3 or tuple(m.shape) != tuple(x.shape[:2]):
            return None
        return m[:, :, None]

    def head_mask(self, x):
        """The mask as [B, 1, S, 1] for a per-head tensor x of shape [B, heads, S, d]."""
        m = self.mask
        if m is None or x.dim() != 4 or m.shape[0] != x.shape[0] or m.shape[1] != x.shape[2]:
            return None
        return m[:, None, :, None]


class FakeQuantLinear(torch.nn.Module):
    """F.linear(fq_act(x), fq_weight(W, per output channel), b); holds the Linear's own
    weight and bias Parameters."""

    def __init__(self, linear, state):
        super().__init__()
        self.weight = linear.weight
        self.bias = linear.bias
        self.in_features, self.out_features = linear.in_features, linear.out_features
        object.__setattr__(self, "_orig", linear)
        object.__setattr__(self, "_state", state)

    def forward(self, x):
        if not self._state.on:
            return F.linear(x, self.weight, self.bias)
        return F.linear(fq_act(x, self._state.seq_mask(x)), fq_weight(self.weight, 0), self.bias)


class FakeQuantEmbedding(torch.nn.Module):
    """F.embedding over fq_weight(table, per tensor); holds the Embedding's own weight."""

    def __init__(self, emb, state):
        super().__init__()
        self.weight = emb.weight
        self.padding_idx = emb.padding_idx
        self.num_embeddings, self.embedding_dim = emb.num_embeddings, emb.embedding_dim
        object.__setattr__(self, "_orig", emb)
        object.__setattr__(self, "_state", state)

    def forward(self, ids):
        w = fq_weight(self.weight, None) if self._state.on else self.weight
        return F.embedding(ids, w, self.padding_idx)


def _attention_forward(self, hidden_states, attention_mask=None, head_mask=None, encoder_hidden_states=None,
                       encoder_attention_mask=None, past_key_value=None, output_attentions=False):
    """Eager BERT self-attention with fq_act on Q, K, V and the probabilities (padded
    query rows excluded from the range); the class's own forward when bypassed."""
    st = self._qat_state
    if not st.on:
        return type(self).forward(self, hidden_states, attention_mask, head_mask, encoder_hidden_states,
                                  encoder_attention_mask, past_key_value, output_attentions)
    if encoder_hidden_states is not None or past_key_value is not None or self.is_decoder:
        raise RuntimeError("qat: only encoder self-attention is emulated")
    q = self.transpose_for_scores(self.query(hidden_states))
    k = self.transpose_for_scores(self.key(hidden_states))
    v = self.transpose_for_scores(self.value(hidden_states))
    hm = st.head_mask(q)
    q, k, v = fq_act(q, hm), fq_act(k, hm), fq_act(v, hm)
    scores = torch.matmul(q, k.transpose(-1, -2)) / math.sqrt(self.attention_head_size)
    if attention_mask is not None:
        scores = scores + attention_mask
    probs = self.dropout(F.softmax(scores, dim=-1))
    if head_mask is not None:
        probs = probs * head_mask
    ctx = torch.matmul(fq_act(probs, hm), v)
    ctx = ctx.permute(0, 2, 1, 3).contiguous()
    ctx = ctx.view(ctx.size()[:-2] + (self.all_head_size,))
    return (ctx, probs) if output_attentions else (ctx,)


def _get(model, path):
    m = model
    for p in path.split("."):
        m = getattr(m, p)
    return m


def _set(model, path, mod):
    parent, _, name = path.rpartition(".")
    setattr(_get(model, parent) if parent else model, name, mod)


def wrap(model):
    """Install the fake quantisers on a BERT sequence classifier in place; returns it."""
    if getattr(model, "_qat_state", None) is not None:
        raise RuntimeError("qat: model is already wrapped")
    if any(isinstance(m, (FakeQuantLinear, FakeQuantEmbedding)) for m in model.modules()):
        raise RuntimeError("qat: model already holds fake-quant modules")
    st = _State()
    lin, tables, attn = targets(model)
    for p in lin:
        _set(model, p, FakeQuantLinear(_get(model, p), st))
    for p in tables:
        _set(model, p, FakeQuantEmbedding(_get(model, p), st))
    for p in attn:
        a = _get(model, p)
        a._qat_state = st
        a.forward = types.MethodType(_attention_forward, a)

    saved = []

    def pre(module, args, kwargs):
        saved.append(st.mask)
        if kwargs.get("attention_mask") is not None:
            st.mask = kwargs["attention_mask"]

    def post(module, args, kwargs, output):
        st.mask = saved.pop() if saved else None

    hooks = [model.register_forward_pre_hook(pre, with_kwargs=True),
             model.register_forward_hook(post, with_kwargs=True, always_call=True)]
    object.__setattr__(model, "_qat_state", st)
    object.__setattr__(model, "_qat_hooks", hooks)
    return model


def unwrap(model):
    """Remove what wrap installed; the model is the plain one again (same Parameters)."""
    st = getattr(model, "_qat_state", None)
    if st is None:
        return model
    for h in model._qat_hooks:
        h.remove()
    lin, tables, attn = targets(model)
    for p in lin + tables:
        _set(model, p, _get(model, p)._orig)
    for p in attn:
        a = _get(model, p)
        del a.forward
        del a._qat_state
    object.__delattr__(model, "_qat_state")
    object.__delattr__(model, "_qat_hooks")
    return model


def is_wrapped(model):
    return getattr(model, "_qat_state", None) is not None


def enabled(model, on):
    """Switch the fake quantisers on, or bypass them (off = the plain fp32 model)."""
    model._qat_state.on = bool(on)
    return model


@contextlib.contextmanager
def bypass(model):
    """The plain fp32 model inside the block; the previous on/off state after it."""
    st = model._qat_state
    prev, st.on = st.on, False
    try:
        yield model
    finally:
        st.on = prev


@contextlib.contextmanager
def mask_context(model, attention_mask):
    """Set the [B, S] attention mask the wrappers use, for a call that does not pass
    `attention_mask` as a keyword to the wrapped model."""
    st = model._qat_state
    prev, st.mask = st.mask, attention_mask
    try:
        yield
    finally:
        st.mask = prev
