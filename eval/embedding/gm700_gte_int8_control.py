"""GM-700 S9 control: gte int8 vs gte fp32 on the model-card pairs (C8, C7 rank-only, logits differ, chunk-1 exactness)."""
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent))
import gm463_bakeoff as B  # noqa: E402
import gm462_ce_enriched as E  # noqa: E402
import rerank_eval as R  # noqa: E402

work = Path(sys.argv[1])
i8 = R.CrossEncoder(work, "ce-gte-mb-int8", 4, rust_like=True)
f32 = R.CrossEncoder(work, "ce-gte-mb", 4, rust_like=True)
print("sha int8", i8.files["model.onnx"], "chunk", i8.chunk)
print("sha fp32", f32.files["model.onnx"], "chunk", f32.chunk)
ok = B.c8_input_format(i8, "ce-gte-mb-int8")
ok7, note = B.c7_known_answer(i8, "ce-gte-mb-int8")
pairs = B.C7_CARDS["ce-gte-mb"][0] + [("where is the config parsed", "fn parse_config(path: &Path) -> Config"),
                                       ("where is the config parsed", "struct Window { w: u32 }")]
a = [float(E.score_texts(i8, q, [d])[0]) for q, d in pairs]
b = [float(E.score_texts(f32, q, [d])[0]) for q, d in pairs]
d = max(abs(x - y) for x, y in zip(a, b))
print("int8", [round(x, 4) for x in a]); print("fp32", [round(x, 4) for x in b])
print(f"max|int8-fp32| {d:.4f} (must be > 1e-3 to tell the arms apart): {'differed' if d > 1e-3 else 'SAME'}")
print("C8", ok, "C7", ok7, note)
sys.exit(0 if ok and ok7 and d > 1e-3 else 1)
