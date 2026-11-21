---- MODULE RequestPath_TTrace_1795234593 ----
EXTENDS RequestPath, Sequences, TLCExt, RequestPath_TEConstants, Toolbox, Naturals, TLC

_expression ==
    LET RequestPath_TEExpression == INSTANCE RequestPath_TEExpression
    IN RequestPath_TEExpression!expression
----

_trace ==
    LET RequestPath_TETrace == INSTANCE RequestPath_TETrace
    IN RequestPath_TETrace!trace
----

_inv ==
    ~(
        TLCGet("level") = Len(_TETrace)
        /\
        permitHeld = (TRUE)
        /\
        phase = ("Send")
        /\
        cur = (d1)
        /\
        cooldownUntil = ((d1 :> 0 @@ d2 :> 0))
        /\
        committed = (FALSE)
        /\
        selectedInCooldown = (FALSE)
        /\
        now = (3)
        /\
        startedAfterDeadline = (TRUE)
        /\
        attempts = (1)
        /\
        replayed = (FALSE)
    )
----

_init ==
    /\ now = _TETrace[1].now
    /\ cooldownUntil = _TETrace[1].cooldownUntil
    /\ startedAfterDeadline = _TETrace[1].startedAfterDeadline
    /\ permitHeld = _TETrace[1].permitHeld
    /\ attempts = _TETrace[1].attempts
    /\ committed = _TETrace[1].committed
    /\ phase = _TETrace[1].phase
    /\ cur = _TETrace[1].cur
    /\ selectedInCooldown = _TETrace[1].selectedInCooldown
    /\ replayed = _TETrace[1].replayed
----

_next ==
    /\ \E i,j \in DOMAIN _TETrace:
        /\ \/ /\ j = i + 1
              /\ i = TLCGet("level")
        /\ now  = _TETrace[i].now
        /\ now' = _TETrace[j].now
        /\ cooldownUntil  = _TETrace[i].cooldownUntil
        /\ cooldownUntil' = _TETrace[j].cooldownUntil
        /\ startedAfterDeadline  = _TETrace[i].startedAfterDeadline
        /\ startedAfterDeadline' = _TETrace[j].startedAfterDeadline
        /\ permitHeld  = _TETrace[i].permitHeld
        /\ permitHeld' = _TETrace[j].permitHeld
        /\ attempts  = _TETrace[i].attempts
        /\ attempts' = _TETrace[j].attempts
        /\ committed  = _TETrace[i].committed
        /\ committed' = _TETrace[j].committed
        /\ phase  = _TETrace[i].phase
        /\ phase' = _TETrace[j].phase
        /\ cur  = _TETrace[i].cur
        /\ cur' = _TETrace[j].cur
        /\ selectedInCooldown  = _TETrace[i].selectedInCooldown
        /\ selectedInCooldown' = _TETrace[j].selectedInCooldown
        /\ replayed  = _TETrace[i].replayed
        /\ replayed' = _TETrace[j].replayed

\* Uncomment the ASSUME below to write the states of the error trace
\* to the given file in Json format. Note that you can pass any tuple
\* to `JsonSerialize`. For example, a sub-sequence of _TETrace.
    \* ASSUME
    \*     LET J == INSTANCE Json
    \*         IN J!JsonSerialize("RequestPath_TTrace_1795234593.json", _TETrace)

=============================================================================

 Note that you can extract this module `RequestPath_TEExpression`
  to a dedicated file to reuse `expression` (the module in the 
  dedicated `RequestPath_TEExpression.tla` file takes precedence 
  over the module `RequestPath_TEExpression` below).

---- MODULE RequestPath_TEExpression ----
EXTENDS RequestPath, Sequences, TLCExt, RequestPath_TEConstants, Toolbox, Naturals, TLC

expression == 
    [
        \* To hide variables of the `RequestPath` spec from the error trace,
        \* remove the variables below.  The trace will be written in the order
        \* of the fields of this record.
        now |-> now
        ,cooldownUntil |-> cooldownUntil
        ,startedAfterDeadline |-> startedAfterDeadline
        ,permitHeld |-> permitHeld
        ,attempts |-> attempts
        ,committed |-> committed
        ,phase |-> phase
        ,cur |-> cur
        ,selectedInCooldown |-> selectedInCooldown
        ,replayed |-> replayed
        
        \* Put additional constant-, state-, and action-level expressions here:
        \* ,_stateNumber |-> _TEPosition
        \* ,_nowUnchanged |-> now = now'
        
        \* Format the `now` variable as Json value.
        \* ,_nowJson |->
        \*     LET J == INSTANCE Json
        \*     IN J!ToJson(now)
        
        \* Lastly, you may build expressions over arbitrary sets of states by
        \* leveraging the _TETrace operator.  For example, this is how to
        \* count the number of times a spec variable changed up to the current
        \* state in the trace.
        \* ,_nowModCount |->
        \*     LET F[s \in DOMAIN _TETrace] ==
        \*         IF s = 1 THEN 0
        \*         ELSE IF _TETrace[s].now # _TETrace[s-1].now
        \*             THEN 1 + F[s-1] ELSE F[s-1]
        \*     IN F[_TEPosition - 1]
    ]

=============================================================================



Parsing and semantic processing can take forever if the trace below is long.
 In this case, it is advised to uncomment the module below to deserialize the
 trace from a generated binary file.

\*
\*---- MODULE RequestPath_TETrace ----
\*EXTENDS RequestPath, IOUtils, RequestPath_TEConstants, TLC
\*
\*trace == IODeserialize("RequestPath_TTrace_1795234593.bin", TRUE)
\*
\*=============================================================================
\*

---- MODULE RequestPath_TETrace ----
EXTENDS RequestPath, RequestPath_TEConstants, TLC

trace == 
    <<
    ([permitHeld |-> FALSE,phase |-> "Select",cur |-> d1,cooldownUntil |-> (d1 :> 0 @@ d2 :> 0),committed |-> FALSE,selectedInCooldown |-> FALSE,now |-> 0,startedAfterDeadline |-> FALSE,attempts |-> 0,replayed |-> FALSE]),
    ([permitHeld |-> FALSE,phase |-> "Select",cur |-> d1,cooldownUntil |-> (d1 :> 0 @@ d2 :> 0),committed |-> FALSE,selectedInCooldown |-> FALSE,now |-> 1,startedAfterDeadline |-> FALSE,attempts |-> 0,replayed |-> FALSE]),
    ([permitHeld |-> FALSE,phase |-> "Select",cur |-> d1,cooldownUntil |-> (d1 :> 0 @@ d2 :> 0),committed |-> FALSE,selectedInCooldown |-> FALSE,now |-> 2,startedAfterDeadline |-> FALSE,attempts |-> 0,replayed |-> FALSE]),
    ([permitHeld |-> FALSE,phase |-> "Admit",cur |-> d1,cooldownUntil |-> (d1 :> 0 @@ d2 :> 0),committed |-> FALSE,selectedInCooldown |-> FALSE,now |-> 2,startedAfterDeadline |-> FALSE,attempts |-> 0,replayed |-> FALSE]),
    ([permitHeld |-> FALSE,phase |-> "Admit",cur |-> d1,cooldownUntil |-> (d1 :> 0 @@ d2 :> 0),committed |-> FALSE,selectedInCooldown |-> FALSE,now |-> 3,startedAfterDeadline |-> FALSE,attempts |-> 0,replayed |-> FALSE]),
    ([permitHeld |-> TRUE,phase |-> "Build",cur |-> d1,cooldownUntil |-> (d1 :> 0 @@ d2 :> 0),committed |-> FALSE,selectedInCooldown |-> FALSE,now |-> 3,startedAfterDeadline |-> FALSE,attempts |-> 0,replayed |-> FALSE]),
    ([permitHeld |-> TRUE,phase |-> "Gate",cur |-> d1,cooldownUntil |-> (d1 :> 0 @@ d2 :> 0),committed |-> FALSE,selectedInCooldown |-> FALSE,now |-> 3,startedAfterDeadline |-> FALSE,attempts |-> 0,replayed |-> FALSE]),
    ([permitHeld |-> TRUE,phase |-> "Send",cur |-> d1,cooldownUntil |-> (d1 :> 0 @@ d2 :> 0),committed |-> FALSE,selectedInCooldown |-> FALSE,now |-> 3,startedAfterDeadline |-> TRUE,attempts |-> 1,replayed |-> FALSE])
    >>
----


=============================================================================

---- MODULE RequestPath_TEConstants ----
EXTENDS RequestPath

CONSTANTS d1, d2

=============================================================================

---- CONFIG RequestPath_TTrace_1795234593 ----
CONSTANTS
    MaxAttempts = 3
    Deadline = 3
    MaxTime = 4
    Deps = { d1 , d2 }
    d2 = d2
    d1 = d1

INVARIANT
    _inv

CHECK_DEADLOCK
    \* CHECK_DEADLOCK off because of PROPERTY or INVARIANT above.
    FALSE

INIT
    _init

NEXT
    _next

CONSTANT
    _TETrace <- _trace

ALIAS
    _expression
=============================================================================
\* Generated on Sat Nov 21 04:16:34 UTC 2026