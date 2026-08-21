# Claude Code multi-agent pack

Unzip this archive at your **repository root**. It creates:

```text
.claude/agents/
  orchestrator.md
  proposer.md
  critic.md
  builder.md
  reviewer.md
```

Claude Code loads project subagents from `.claude/agents/`. Each file already has YAML frontmatter (`name`, `description`).

## Install

```bash
cd /path/to/your/repo
unzip /path/to/claude-agents-pack.zip
# if the zip contains a top-level folder, copy .claude into the repo root instead:
# cp -R claude-agents-pack/.claude .
```

Commit `.claude/agents/` so the team shares the same agents.

## Start a task

In Claude Code:

```text
Use the orchestrator agent. Run Proposer, then Critic, then Builder, then Reviewer in order. Do not skip phases. Write explanations in complete sentences.

Task: <describe your task here>
```

Or invoke agents by name, for example:

```text
Use the proposer subagent to produce three solutions for: <brief>
```

## Order

1. Orchestrator writes the problem brief.  
2. Proposer returns three solutions.  
3. Critic returns a decision record.  
4. Builder implements the decision record.  
5. Reviewer reviews, fixes Blockers and Majors, and issues a verdict.
