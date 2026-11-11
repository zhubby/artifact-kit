# Third-party visual components

Artifact Kit includes adapted React Bits components by David Haz:

- Repository: https://github.com/DavidHDev/react-bits
- Source commit: `9d9c687dc4f55b23539aa5417b703078047efa74`
- License: **MIT + Commons Clause License Condition v1.0**. The complete upstream license is retained verbatim in `src/components/react-bits/LICENSE.md`.
- Component source paths and original SHA-256 hashes are recorded in `src/components/react-bits/provenance.json`.

Adaptations:

- **Aurora:** original WebGL2 shaders and color processing; adds a frame-rate limit, static fallback, immediate offscreen/visibility cancellation and context-loss handling. Deletes owned GL resources without forcing context loss, so React StrictMode can reinitialize the same canvas.
- **SpotlightCard:** retains radial pointer illumination and masked edge glow; uses local pointer events and one coalesced frame per movement instead of per-card global listeners, ambient loops and grain.
- **AnimatedContent:** retains GSAP context and staggered entrance; limits the API to mount/route entrance, with shared live reduced-motion control, immediately reversible cleanup and no replay on data or language updates. Server-rendered content stays visible before JavaScript runs.

The components are integrated into the Artifact Kit application. The upstream license includes additional restrictions on selling or redistributing the components themselves; it is not an unmodified MIT license.
