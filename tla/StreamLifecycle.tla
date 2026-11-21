------------------------- MODULE StreamLifecycle -------------------------
(* Streaming lifecycle of Executor::attempt + hold_permit. Setup ends when the
   first complete event arrives; from then on bytes are committed to the client
   and the only retry-free exits are end, idle timeout, upstream error, or
   client disconnect. The permit lives as long as the stream value. *)
EXTENDS Naturals
VARIABLES state, committed, permit, retried, cooldownHint
vars == <<state, committed, permit, retried, cooldownHint>>

Init == state = "Setup" /\ committed = FALSE /\ permit = TRUE
        /\ retried = FALSE /\ cooldownHint = FALSE

FirstEvent == /\ state = "Setup" /\ state' = "Streaming" /\ committed' = TRUE
              /\ UNCHANGED <<permit, retried, cooldownHint>>
SetupFail  == /\ state = "Setup" /\ state' = "Retry" /\ permit' = FALSE
              /\ UNCHANGED <<committed, retried, cooldownHint>>
RetryNext  == /\ state = "Retry" /\ ~committed /\ retried' = TRUE /\ state' = "Closed"
              /\ UNCHANGED <<committed, permit, cooldownHint>>
Event      == /\ state = "Streaming" /\ UNCHANGED vars
End        == /\ state = "Streaming" /\ state' = "Closed" /\ permit' = FALSE
              /\ UNCHANGED <<committed, retried, cooldownHint>>
IdleTimeout == /\ state = "Streaming" /\ state' = "Closed" /\ permit' = FALSE
               /\ UNCHANGED <<committed, retried, cooldownHint>>
MidStreamError == /\ state = "Streaming" /\ state' = "Closed" /\ permit' = FALSE
                  /\ \/ cooldownHint' = TRUE \/ UNCHANGED cooldownHint
                  /\ UNCHANGED <<committed, retried>>
ClientDisconnect == /\ state \in {"Setup", "Streaming"} /\ state' = "Closed"
                    /\ permit' = FALSE /\ UNCHANGED <<committed, retried, cooldownHint>>
Done == state = "Closed" /\ UNCHANGED vars

Next == FirstEvent \/ SetupFail \/ RetryNext \/ Event \/ End \/ IdleTimeout
        \/ MidStreamError \/ ClientDisconnect \/ Done
Spec == Init /\ [][Next]_vars

\* (i) retry never happens after a byte was committed.
NoRetryAfterCommit == committed => ~retried
\* (iii) permit released on every terminal path.
PermitReleasedWhenClosed == state = "Closed" => ~permit
=========================================================================
