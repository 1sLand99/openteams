---
id: literature_research_team
name: Literature Research Team
description: Browser-based literature research with domain-aware source selection, evidence synthesis, and independent review.
members:
- preset_id: literature-review-researcher
  id: literature-review-researcher
  name: Literature Review Researcher
- preset_id: literature-research-assistant
  id: literature-research-assistant
  name: Literature Research Assistant
- preset_id: literature-research-assistant
  id: literature-research-assistant-2
  name: Literature Research Assistant 2
- preset_id: literature-research-assistant
  id: literature-research-assistant-3
  name: Literature Research Assistant 3
- preset_id: literature-research-assistant
  id: literature-research-assistant-4
  name: Literature Research Assistant 4
- preset_id: literature-research-assistant
  id: literature-research-assistant-5
  name: Literature Research Assistant 5
- preset_id: literature-research-reviewer
  id: literature-research-reviewer
  name: Literature Research Reviewer
lead_member_id: literature-review-researcher
workflow_steps:
- title: Scope and plan
  description: Define the research question, boundaries, priority databases, venues, and evidence needs.
- title: Search and synthesize
  description: Search public literature with browser tools, verify key sources, and synthesize findings with their limits.
- title: Review and deliver
  description: Independently check decisive claims and deliver a corrected, traceable research result.
tier: standard
enabled: true
---

Use browser-verified public literature to answer the user's research question.
- The Researcher scopes the work, assigns searches, and synthesizes the evidence.
- The Research Assistants search relevant authoritative databases and top venues; every delivered paper includes title, link, publication date, core contribution, and relevance to the topic.
- The Reviewer independently verifies decisive sources and returns `PASS`, `NEEDS_REPAIR`, or `ESCALATE`.
- Disclose evidence gaps and access limits; final acceptance belongs to the user.
