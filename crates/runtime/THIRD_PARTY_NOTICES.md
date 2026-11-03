# Third-Party Notices

## minutes whisper-guard

`src/transcript_cleanup.rs` includes rules and phrase lists adapted from the whisper-guard crate in minutes.

- Upstream project: https://github.com/silverstein/minutes
- Pinned revision: `fa563b65321b4571aa33ff3399b059f32849ccef`
- Upstream source path: `crates/whisper-guard/src/segments.rs`
- Copyright: Copyright (c) 2026 Mat Silverstein

Local modifications apply the rules to sentence units inside one recognized final instead of transcript lines, remove noise markers instead of collapsing them behind annotations, drop "see you next time" and the "transcribed by" and "translated by" prefixes from the hallucination lists, omit the URL, foreign-script and trailing-command rules, and report content-free counts in place of log lines.

### License

MIT License

Copyright (c) 2026 Mat Silverstein

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
