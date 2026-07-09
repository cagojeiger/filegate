# CLAUDE.md

Behavioral guidelines to reduce common LLM coding mistakes. Merge with project-specific instructions as needed.

**Tradeoff:** These guidelines bias toward caution over speed. For trivial tasks, use judgment.

## 1. Think Before Coding

**Don't assume. Don't hide confusion. Surface tradeoffs.**

Before implementing:
- State your assumptions explicitly. If uncertain, ask.
- If multiple interpretations exist, present them - don't pick silently.
- If a simpler approach exists, say so. Push back when warranted.
- If something is unclear, stop. Name what's confusing. Ask.

## 2. Simplicity First

**Minimum code that solves the problem. Nothing speculative.**

- No features beyond what was asked.
- No abstractions for single-use code.
- No "flexibility" or "configurability" that wasn't requested.
- No error handling for impossible scenarios.
- If you write 200 lines and it could be 50, rewrite it.

Ask yourself: "Would a senior engineer say this is overcomplicated?" If yes, simplify.

## 3. Surgical Changes

**Touch only what you must. Clean up only your own mess.**

When editing existing code:
- Don't "improve" adjacent code, comments, or formatting.
- Don't refactor things that aren't broken.
- Match existing style, even if you'd do it differently.
- If you notice unrelated dead code, mention it - don't delete it.

When your changes create orphans:
- Remove imports/variables/functions that YOUR changes made unused.
- Don't remove pre-existing dead code unless asked.

The test: Every changed line should trace directly to the user's request.

## 4. Goal-Driven Execution

**Define success criteria. Loop until verified.**

Transform tasks into verifiable goals:
- "Add validation" → "Write tests for invalid inputs, then make them pass"
- "Fix the bug" → "Write a test that reproduces it, then make it pass"
- "Refactor X" → "Ensure tests pass before and after"

For multi-step tasks, state a brief plan:
```
1. [Step] → verify: [check]
2. [Step] → verify: [check]
3. [Step] → verify: [check]
```

Strong success criteria let you loop independently. Weak criteria ("make it work") require constant clarification.

---

## 5. Commit Convention (MANDATORY)

**All commits MUST follow the [Udacity Git Commit Message Style Guide](https://udacity.github.io/git-styleguide/). No exceptions.**

### Message structure

```
type: Subject

body

footer
```

- Separate **subject** from **body** with a blank line.
- **type** is required and lowercase; **Subject** follows after `: `.

### Type (required, one of)

| type | use for |
|------|---------|
| `feat`     | a new feature |
| `fix`      | a bug fix |
| `docs`     | documentation only |
| `style`    | formatting, white-space — no code-meaning change |
| `refactor` | code change that neither fixes a bug nor adds a feature |
| `test`     | adding or correcting tests |
| `chore`    | build process, tooling, deps, scaffolding |

### Subject line rules

- Use the **imperative mood**: "Add", not "Added"/"Adds".
- **Capitalize** the first letter.
- **No period** at the end.
- Limit to **~50 characters**.

### Body & footer (optional)

- Wrap the body at **72 characters**.
- Explain **what** and **why**, not how.
- Footer for issue references / breaking changes (e.g. `Closes #123`).

### Examples

```
feat: Add copy-baseline script for release snapshots
fix: Pin worker image to digest in 2026-06 release
docs: Clarify baseline vs release distinction
chore: Scaffold repo skeleton (baseline, releases)
```

---

**These guidelines are working if:** fewer unnecessary changes in diffs, fewer rewrites due to overcomplication, and clarifying questions come before implementation rather than after mistakes.
