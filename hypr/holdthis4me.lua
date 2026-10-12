-- holdthis4me shelf: slides out from the left edge. The 2 px trigger strip on the left
-- edge is invisible and never animates.
hl.layer_rule({
    match = { namespace = "holdthis4me" },
    animation = "slide left",
})
hl.layer_rule({
    match = { namespace = "holdthis4me-trigger" },
    no_anim = true,
})
