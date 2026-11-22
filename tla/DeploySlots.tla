--------------------------- MODULE DeploySlots ---------------------------
(* llmux2-deploy handoff between slots a and b with SO_REUSEPORT. The handoff
   starts the idle slot, waits healthy, then stops (drains) the old one. *)
EXTENDS Naturals
Slots == {"a", "b"}
VARIABLES slot, accepting, phase
vars == <<slot, accepting, phase>>
\* slot[s] in stopped | starting | active | draining
Init == slot = [s \in Slots |-> IF s = "a" THEN "active" ELSE "stopped"]
        /\ accepting = [s \in Slots |-> s = "a"] /\ phase = "idle"

Start(s) == /\ phase = "idle" /\ slot[s] = "stopped"
            /\ \E o \in Slots : o # s /\ slot[o] = "active"
            /\ slot' = [slot EXCEPT ![s] = "starting"] /\ phase' = "handoff"
            /\ UNCHANGED accepting
Healthy(s) == /\ slot[s] = "starting" /\ slot' = [slot EXCEPT ![s] = "active"]
              /\ accepting' = [accepting EXCEPT ![s] = TRUE] /\ UNCHANGED phase
Abort(s) == /\ slot[s] = "starting" /\ slot' = [slot EXCEPT ![s] = "stopped"]
            /\ phase' = "idle" /\ UNCHANGED accepting
Drain(s) == /\ phase = "handoff" /\ slot[s] = "active"
            /\ \E o \in Slots : o # s /\ slot[o] = "active"
            /\ slot' = [slot EXCEPT ![s] = "draining"]
            /\ accepting' = [accepting EXCEPT ![s] = FALSE] /\ UNCHANGED phase
Retire(s) == /\ slot[s] = "draining" /\ slot' = [slot EXCEPT ![s] = "stopped"]
             /\ phase' = "idle" /\ UNCHANGED accepting
Next == \E s \in Slots : Start(s) \/ Healthy(s) \/ Abort(s) \/ Drain(s) \/ Retire(s)
Spec == Init /\ [][Next]_vars

\* (v) at least one slot accepting at all times.
AlwaysOneAccepting == \E s \in Slots : accepting[s]
\* (v) a draining slot never accepts new connections.
DrainingNotAccepting == \A s \in Slots : slot[s] = "draining" => ~accepting[s]
AcceptingIsActive == \A s \in Slots : accepting[s] => slot[s] = "active"
=========================================================================
