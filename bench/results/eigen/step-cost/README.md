# One Lanczos step at 1M nodes: SpMV against the rest (DESIGN.md §12)

`bench/eigen.py --step-cost --steps 200 --graph random --nodes 1000000
--cases normalized_laplacian:smallest --precision f32,f64 --threads 1,2,4`.
`before-…` built at 5611b0d (before the basis-pass levers), `after-…` at a359b5a
(eight-lane dot products, chunked rotation). Same 4-vCPU VM, run back to back
with the scaling runs stopped; the load average (2–4, mostly the run itself) is
recorded per timing.
