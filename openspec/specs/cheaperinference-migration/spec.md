# cheaperinference-migration

## Purpose

Moving CheaperInference traffic from OmniRoute onto llmux2.

## Requirements

### Requirement: Exact API compatibility

The gateway SHALL match the request arguments and response shapes
CheaperInference requires, exactly.

### Requirement: Complete migration before switchover

All OmniRoute traffic SHALL be migrated, with nothing dropped, before
llmux2 takes over; real OmniRoute traffic is the reference for coverage.

### Requirement: Revenue-oriented allocation

Provider and capacity allocation SHALL favor increasing CheaperInference
revenue.

### Requirement: No OmniRoute investment

Work SHALL go into the llmux2 replacement, not into improving the OmniRoute
server itself.
