"""GM-461 control 4: the numpy IRLS fit of gm461_learned_verdict.py equals scikit-learn's L2 logistic regression.

Reads the .npz written by `gm461_learned_verdict.py --dump-fit`; needs numpy + scikit-learn only, so it runs
under an interpreter without the eval stack (e.g. g-mesh-data/gm436/venv).

  <venv>/bin/python -I gm461_sklearn_check.py <fit.npz> [--tol 1e-4]
"""
import argparse
import sys

import numpy as np
from sklearn.linear_model import LogisticRegression


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("npz")
    ap.add_argument("--tol", type=float, default=1e-4)
    a = ap.parse_args()
    d = np.load(a.npz)
    fits = sorted({k.rsplit("_", 1)[0] for k in d.files})
    bad = 0
    for f in fits:
        Z, y, lam, w = d[f"{f}_Z"], d[f"{f}_y"], float(d[f"{f}_lam"]), d[f"{f}_w"]
        # sum log-loss + lam/2 ||coef||^2 == sklearn's C * sum log-loss + 1/2 ||coef||^2 with C = 1/lam
        m = LogisticRegression(C=1.0 / lam, tol=1e-12, max_iter=100000).fit(Z, y.astype(int))
        sk = np.concatenate([m.intercept_, m.coef_[0]])
        diff = float(np.max(np.abs(sk - w)))
        ok = diff <= a.tol
        bad += not ok
        print(f"{f}: lambda {lam}, max |numpy - sklearn| = {diff:.2e} {'ok' if ok else 'MISMATCH'}")
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
