# Third-Party Notices

## React Bits Focus Scenes

This application includes adapted source from the React Bits components SoftAurora, Silk, Threads, Prism, and Orb.

- Upstream project: https://github.com/DavidHDev/react-bits
- Prism component page: https://reactbits.dev/backgrounds/prism
- Orb component page: https://reactbits.dev/backgrounds/orb
- Pinned revision: `1320d40a8318ac7d4fe6690c7206ceda8cdd59bd`
- Upstream source paths: `src/ts-default/Backgrounds/{SoftAurora,Silk,Threads,Prism,Orb}`
- Copyright: Copyright (c) 2026 David Haz

Local modifications adapt the components to the Conversation Runtime desktop application. They add the shared Voice Focus state and bounded intensity contract, disable pointer and mouse interaction, pause rendering while the document is hidden, lazy-load each built-in scene, fall back to Still Gradient for reduced motion and initialization/load failures, release animation/listener/canvas/WebGL resources on cleanup, and make Orb itself the central voice presence.

Silk retains the pinned shader, uniforms, camera framing, device-pixel-ratio bounds, and plane scaling while replacing React Three Fiber Canvas orchestration with a directly owned Three.js renderer, scene, camera, mesh, and visibility-aware animation loop so setup and render failures have a deterministic component fallback and cleanup path.

### License

MIT + Commons Clause License Condition v1.0

Copyright (c) 2026 David Haz

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, and distribute the Software **as part of an application, website, or product**, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

## Commons Clause Restriction

You may use this Software, including for any commercial purpose, **so long as you do not sell, sublicense, or redistribute the components themselves-whether alone, in a bundle, or as a ported version.**

## No Warranty

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
