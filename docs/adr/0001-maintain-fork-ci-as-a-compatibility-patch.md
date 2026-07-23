# Maintain fork CI as a compatibility patch

The fork will adapt upstream validation workflows in place while preserving their jobs, steps, and
check names, limiting divergence to runner selection, gates, and repository guards. A parallel
Electivus suite would duplicate validation logic and drift, while infrastructure parity would
couple the fork to private OpenAI resources.
