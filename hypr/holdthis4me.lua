-- holdthis4me: the shelf animates itself (drops in from the top, rises back out), so
-- Hyprland must not add its own layer animation on top. The trigger surfaces (corner
-- square, left-edge strip) are invisible and never animate either.
hl.layer_rule({
    match = { namespace = "holdthis4me" },
    no_anim = true,
})
hl.layer_rule({
    match = { namespace = "holdthis4me-trigger" },
    no_anim = true,
})
