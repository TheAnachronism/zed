---
title: Snippets - Zed
description: Create and use personal and project code snippets in Zed with tab stops, placeholders, variables, and language-scoped triggers.
---

# Snippets

Zed supports two kinds of snippets:

- **Personal snippets** stored in your config directory and available everywhere.
- **Project snippets** stored in `.zed/snippets/` inside a project folder and shared through the project's files.

Both use the same JSON format and [scopes](#scopes).

## Personal snippets

Use the {#action snippets::ConfigureSnippets} action to create a new snippets file or edit an existing snippets file for a specified [scope](#scopes).

The snippets are located in `~/.config/zed/snippets` directory to which you can navigate with the {#action snippets::OpenFolder} action.

## Project snippets

Store project snippets in `.zed/snippets/` at the root of an opened folder:

- `.zed/snippets/snippets.json` for all languages
- `.zed/snippets/<language>.json` for a specific language (same filenames as [personal scopes](#scopes))

Edit these files directly in your project. There is no separate command for project snippets.

Project snippets apply only when you edit a **saved file** whose path is inside a **trusted** opened folder. They are not offered for untitled buffers or while a project is in **Restricted Mode**.

When a project has multiple opened folder roots, Zed uses snippets from the **nearest** root that contains the file you are editing.

If a project snippet and a personal snippet share the same trigger, both appear in completions with their sources labeled. The project snippet is listed first.

If a project snippet file contains invalid JSON, Zed shows an error on that file. Snippets from other valid files in the project still work.

Changes to project snippet files take effect as soon as you save them; you do not need to restart Zed.

### Sharing with your team

Commit `.zed/snippets/` to version control (for example, in Git) to share snippets with everyone working in the repository. Each teammate gets the same project snippets when they open the folder and trust the project.

Joining a live collaboration session does not import the host's project snippets. To use them, open a copy of the project files yourself.

In a [multi-root project](./windows-and-projects.md#adding-folders-to-a-project), add a `.zed/snippets/` directory under each root that needs its own snippets. Files under a root apply only to saved files within that root.

## Example configuration

```json
{
  // Each snippet must have a name and body, but the prefix and description are optional.
  // The prefix is used to trigger the snippet, but when omitted then the name is used.
  // Use placeholders like $1, $2 or ${1:defaultValue} to define tab stops.
  // The $0 determines the final cursor position.
  // Placeholders with the same value are linked.
  // If the snippet contains the $ symbol outside of a placeholder, it must be escaped with two slashes (e.g. \\$var).
  "Log to console": {
    "prefix": "log",
    "body": ["console.info(\"Hello, ${1:World}!\")", "$0"],
    "description": "Logs to console"
  }
}
```

Choice placeholders such as `${2|tags,creators,parodies|}` show a list of options. Use the Up and Down arrow keys to select an option, then Enter to insert it.

Use `${1/(.*)/${1:/downcase}/}` to insert a lowercase copy of the first placeholder. The copy updates as you edit the placeholder. For example, a snippet body of `[${1}](/${2|tags,creators,parodies,sources,categories|}/${1/(.*)/${1:/downcase}/}/)` creates a link whose final path segment follows the link text in lowercase.

## Scopes

The scope is determined by the language name in lowercase e.g. `python.json` for Python, `shell script.json` for Shell Script, but there are some exceptions to this rule:

| Scope      | Filename        |
| ---------- | --------------- |
| Global     | snippets.json   |
| JSX        | javascript.json |
| Plain Text | plaintext.json  |

To create JSX snippets you have to use `javascript.json` snippets file, instead of `jsx.json`, but this does not apply to TSX and TypeScript which follow the above rule.

Path separators (`/` and `\`) are removed from the file name, so a language named `PL/X` uses `plx.json`.

These filenames apply to both personal snippets in `~/.config/zed/snippets/` and project snippets in `.zed/snippets/`.

## Known Limitations

- Only the first prefix is used when a list of prefixes is passed in.
- Currently only the `json` snippet file format is supported.
