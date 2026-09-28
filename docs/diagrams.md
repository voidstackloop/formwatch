# formwatch diagrams

Rendered copies live in [`docs/images/`](images/). Background for these
diagrams: [ADR-0001](adr/0001-formwatch-architecture.md) (architecture) and
[ADR-0002](adr/0002-enterprise-hardening.md) (authorized-use hardening).

## Check pipeline

From a form registry to the places results end up.

![formwatch pipeline](images/diagram-pipeline.svg)

```mermaid
flowchart LR
    subgraph IN["Input"]
        URL["formwatch test &lt;url&gt;"]
        YML["forms.yml<br/>(one or more, globs ok)"]
        CFG["formwatch.yml<br/>options · proxy · limits"]
    end

    subgraph RUN["runner (tokio)"]
        SHARD["shard + max-concurrent<br/>per-host rate limiter"]
        CHROME["headless Chrome<br/>via chromiumoxide (CDP)"]
    end

    subgraph CHECKS["checks — one CheckResult each (Pass · Warn · Fail)"]
        direction TB
        C1["Flow · Submission wizard · required docs ·<br/>validation errors · input persistence"]
        C2["Accessibility · axe-core injected ·<br/>zoom · link text · duplicate names"]
        C3["Mobile · 375px emulation · tap targets ·<br/>autofill hints · input types · bot wall"]
        C8["Custom checks (--checks-dir) ·<br/>LLM wording review, opt-in, PII redacted"]
    end

    subgraph STATE[".formwatch/"]
        HIST["history/&lt;form&gt;/&lt;ts&gt;.json<br/>diffs · flakiness"]
        BASE["baseline.json<br/>accepted findings"]
        AUDIT["audit log (JSONL)"]
    end

    subgraph OUT["Outputs"]
        TERM["terminal table"]
        HTML["report --html<br/>with failure screenshots"]
        CI["JUnit · SARIF · JSON"]
        PROM["serve: /metrics · /api/forms"]
        HOOK["webhook<br/>Slack / generic JSON"]
        GHA["GitHub Action<br/>code scanning · PR gate"]
    end

    URL --> SHARD
    YML --> SHARD
    CFG --> SHARD
    SHARD --> CHROME --> CHECKS
    CHECKS --> HIST
    BASE -. "only new or worse findings fail" .-> CHECKS
    CHECKS --> AUDIT
    CHECKS --> TERM
    HIST --> HTML
    HIST --> CI
    HIST --> PROM
    HIST -- "regression vs previous run" --> HOOK
    CI --> GHA
```

## One `formwatch test` run

```mermaid
sequenceDiagram
    autonumber
    actor U as User / CI
    participant F as formwatch
    participant C as Chrome (CDP)
    participant S as Target form
    participant H as .formwatch/history

    U->>F: formwatch test https://city.gov/apply
    F->>F: load config · check authorized-use flags
    F->>C: launch headless (cached build or fetch)
    C->>S: navigate
    loop each check category
        F->>C: evaluate JS / emulate device / dispatch input
        C-->>F: DOM facts, axe-core violations
        F->>F: CheckResult (Pass · Warn · Fail) + screenshot on non-Pass
    end
    Note over F,S: Real POST only with --submit --accept-terms
    F->>H: write run JSON
    F->>H: read previous run → diff / flaky
    F-->>U: table · exit code (--fail-on fail|warn)
```
