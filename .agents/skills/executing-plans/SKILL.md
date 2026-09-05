---
name: executing-plans
description: Use when you have a written implementation plan to execute in a separate session with review checkpoints
---

# Executing Plans

## Overview

Load plan, review critically, execute all tasks, report when complete.

**Announce at start:** "I'm using the executing-plans skill to implement this plan."

Use relevant Superpowers helper skills when available, applicable, and permitted. Otherwise perform equivalent setup, implementation, verification, and handoff directly; missing helper skills alone do not block the task. Delegate only when authorized and useful for independent work.

## The Process

### Step 1: Load and Review Plan
1. Ensure an isolated workspace: use superpowers:using-git-worktrees when available, or create or verify one directly without discarding user work
2. Read plan file
3. Review critically - identify any questions or concerns about the plan
4. Resolve routine concerns from available evidence. Ask before changing the approved outcome, architecture, approval boundaries, or explicitly required method; preserve explicit review checkpoints
5. Track the plan items and proceed with authorized work that does not depend on an unresolved decision

### Step 2: Execute Tasks

For each task:
1. Mark as in_progress
2. Follow the intended behavior, constraints, and explicit review checkpoints. Resolve routine implementation details from current repository evidence and report material deviations
3. Run verifications as specified
4. Mark as completed

### Step 3: Complete Development

After all tasks complete and verified:
- Use superpowers:finishing-a-development-branch when available and applicable; otherwise verify the result and perform the requested handoff directly
- Carry out the handoff already authorized by the user. Present choices only when a material handoff decision remains unresolved
- Do not infer authorization to merge, publish, or discard work

## When to Stop and Ask for Help

When verification fails or an instruction appears unclear, first inspect the relevant code, configuration, plan, and error output. Diagnose and fix problems within the authorized scope, then rerun the relevant check. Do not repeat the same failed attempt without new evidence.

Ask only when progress requires a material decision that cannot be resolved from available evidence, additional approval, or an unavailable prerequisite with no permitted alternative. Pause only dependent work and continue useful independent work. Do not invent requirements or install unapproved dependencies.

If verification remains blocked, report the exact command, result, and unresolved check. Distinguish implementation completed with verification blocked from fully verified completion; preserve PR, review, and release gates.

## When to Revisit Earlier Steps

**Return to Review (Step 1) when:**
- Partner updates the plan based on your feedback
- Fundamental approach needs rethinking

Do not bypass a genuine blocker; use the diagnosis and clarification rules above.

## Remember
- Review plan critically first
- Preserve the plan's intended behavior, constraints, and explicit checkpoints
- Don't skip verifications
- Use available helper skills where applicable; otherwise perform equivalent work directly
- Investigate routine failures; ask when a material decision or additional authorization is required
- Never start implementation on main/master branch without explicit user consent
