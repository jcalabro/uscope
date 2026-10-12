# Documentation

Public documentation lives in `docs/` and is published with VitePress.
Developer documentation lives in `dev/`. Design and implementation records
stay in `plans/`.

## Audience

Public pages explain how to use uscope. They may explain a design choice when
it affects a user's result, but they do not describe internal module structure,
test machinery, or implementation plans.

Developer pages explain how to change and verify the project. `AGENTS.md` is a
short router and a list of rules that apply to almost every change. Put
area-specific details here instead of growing the top-level file.

Plans record decisions, evidence, and implementation history. They are not a
promise of current behavior and are not included in the public site.

## Writing

- Open with what the reader can do or what the component does.
- Use common words and short sentences.
- Define a technical term the first time it is needed.
- Use one name for one concept.
- Prefer a command and its output over a broad feature claim.
- State support and limits exactly.
- Link to the owner of reference material instead of copying it.
- Keep examples complete enough to run.
- Avoid filler, sales language, and claims such as “seamless,” “powerful,” or
  “comprehensive.”
- Use descriptive headings such as “Attach to a running process.”

If the product has no answer yet, say so and record what must exist before the
page can be completed:

```markdown
> TODO: uscope does not have a supported installation method yet. Document
> installation here after release artifacts or packages are available.
```

Do not invent a workflow to make a page appear complete.

## Tested examples

Examples in `docs/expressions.md`, `docs/views.md`,
`docs/writing-views.md`, and `docs/visualizers.md` are test inputs. Keep their
fence names and expected-output syntax intact. Change the reference and the
implementation in the same commit.

## Site

VitePress configuration lives in `docs/.vitepress`. Keep source pages as plain
Markdown with relative links so they remain readable in GitHub and by agents.
Use VitePress components only when ordinary Markdown cannot express the page.

Run the documentation check with:

```sh
just docs
```

It builds the static site and catches broken internal links.
