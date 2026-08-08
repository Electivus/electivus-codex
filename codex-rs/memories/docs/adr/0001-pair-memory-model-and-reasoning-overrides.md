# Pair memory model and reasoning-effort overrides

Memory extraction and consolidation expose separate
`extract_model_reasoning_effort` and `consolidation_model_reasoning_effort` settings alongside
their existing model overrides. An effort override requires the corresponding model override in
the effective merged configuration, while a model override may stand alone and retain the existing
phase default (`low` for extraction and `medium` for consolidation); configured effort values are
sent unchanged so providers remain authoritative and future effort values remain compatible.
