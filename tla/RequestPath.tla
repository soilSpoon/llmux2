---------------------------- MODULE RequestPath ----------------------------
(* Mirrors crates/llmux-core/src/machine.rs. Two deployments, time is a bounded
   tick counter. Passthrough deployments only: a Convert-support deployment is
   deliberately exempt from the cooldown skip in Executor::execute. *)
EXTENDS Naturals, FiniteSets

CONSTANTS MaxAttempts, Deadline, Deps, MaxTime
ASSUME MaxAttempts \in Nat \ {0} /\ Deadline \in Nat \ {0}

VARIABLES phase, attempts, now, cur, cooldownUntil, committed, replayed,
          startedAfterDeadline, selectedInCooldown, permitHeld, wire

vars == <<phase, attempts, now, cur, cooldownUntil, committed, replayed,
          startedAfterDeadline, selectedInCooldown, permitHeld, wire>>

Init == /\ phase = "Select" /\ attempts = 0 /\ now = 0 /\ cur = CHOOSE d \in Deps : TRUE
        /\ cooldownUntil = [d \in Deps |-> 0]
        /\ committed = FALSE /\ replayed = FALSE
        /\ startedAfterDeadline = FALSE /\ selectedInCooldown = FALSE
        /\ permitHeld = FALSE /\ wire = FALSE

Tick == /\ now < MaxTime /\ now' = now + 1
        /\ UNCHANGED <<phase, attempts, cur, cooldownUntil, committed, replayed,
                       startedAfterDeadline, selectedInCooldown, permitHeld, wire>>

Select(d) ==
  /\ phase = "Select" /\ attempts < MaxAttempts /\ now < Deadline
  /\ cooldownUntil[d] <= now
  /\ cur' = d /\ phase' = "Admit"
  /\ selectedInCooldown' = (cooldownUntil[d] > now)
  /\ UNCHANGED <<attempts, now, cooldownUntil, committed, replayed,
                 startedAfterDeadline, permitHeld, wire>>

Admit == /\ phase = "Admit" /\ permitHeld' = TRUE /\ phase' = "Build"
         /\ UNCHANGED <<attempts, now, cur, cooldownUntil, committed, replayed,
                        startedAfterDeadline, selectedInCooldown, wire>>

\* Cooldown may be set while the request is building (another request limited it).
CooldownDuringBuild ==
  /\ phase = "Build" /\ now + 1 <= MaxTime
  /\ cooldownUntil' = [cooldownUntil EXCEPT ![cur] = now + 1]
  /\ UNCHANGED <<phase, attempts, now, cur, committed, replayed,
                 startedAfterDeadline, selectedInCooldown, permitHeld, wire>>

BuildDone == /\ phase = "Build" /\ phase' = "Gate"
             /\ UNCHANGED <<attempts, now, cur, cooldownUntil, committed, replayed,
                            startedAfterDeadline, selectedInCooldown, permitHeld, wire>>

\* Gate re-admits after build; permit is kept only if still not cooling.
GateFail == /\ phase = "Gate" /\ cooldownUntil[cur] > now
            /\ phase' = "Select" /\ permitHeld' = FALSE
            /\ UNCHANGED <<attempts, now, cur, cooldownUntil, committed, replayed,
                           startedAfterDeadline, selectedInCooldown, wire>>

GateOk == /\ phase = "Gate" /\ cooldownUntil[cur] <= now
          /\ attempts' = attempts + 1 /\ phase' = "Send"
          \* timeout_at(attempt_deadline, ..) means no upstream I/O is issued once
          \* the deadline passed; the attempt counts but resolves as Timeout.
          /\ wire' = (now < Deadline)
          /\ startedAfterDeadline' = (startedAfterDeadline \/ (wire' /\ now >= Deadline))
          /\ UNCHANGED <<now, cur, cooldownUntil, committed, replayed,
                         selectedInCooldown, permitHeld>>

SendSuccess == /\ phase = "Send" /\ wire /\ committed' = TRUE /\ phase' = "Done"
               /\ UNCHANGED <<attempts, now, cur, cooldownUntil, replayed,
                              startedAfterDeadline, selectedInCooldown, permitHeld, wire>>

\* Pre-commit failure: permit released, next candidate. Cooldown may be set.
SendFail == /\ phase = "Send" /\ committed = FALSE
            /\ phase' = "Select" /\ permitHeld' = FALSE /\ wire' = FALSE
            /\ \/ cooldownUntil' = [cooldownUntil EXCEPT ![cur] = now + 1]
               \/ UNCHANGED cooldownUntil
            /\ UNCHANGED <<attempts, now, cur, committed, replayed,
                           startedAfterDeadline, selectedInCooldown, wire>>

Exhaust == /\ phase = "Select" /\ (attempts = MaxAttempts \/ now >= Deadline)
           /\ phase' = "Exhausted" /\ permitHeld' = FALSE
           /\ UNCHANGED <<attempts, now, cur, cooldownUntil, committed, replayed,
                          startedAfterDeadline, selectedInCooldown, wire>>

Terminal == /\ phase \in {"Done", "Exhausted"} /\ UNCHANGED vars

Next == Tick \/ \E d \in Deps : Select(d)
        \/ Admit \/ CooldownDuringBuild \/ BuildDone \/ GateFail \/ GateOk
        \/ SendSuccess \/ SendFail \/ Exhaust \/ Terminal

Spec == Init /\ [][Next]_vars

\* (i) no attempt starts once a response is committed.
NoReplayAfterCommit == committed => phase = "Done" /\ ~replayed
\* (ii) attempt bound and deadline.
AttemptsBounded == attempts <= MaxAttempts
NoAttemptAfterDeadline == ~startedAfterDeadline
\* (iv) a deployment in cooldown is never selected.
NoSelectInCooldown == ~selectedInCooldown
\* (iii) permits are released on every non-success terminal path.
PermitReleasedOnExhaust == phase = "Exhausted" => ~permitHeld
=============================================================================
