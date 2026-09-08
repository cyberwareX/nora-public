---
state: perceive
mcp: [sibyl-read]
transitions: [bookings/express]
context: { tag_key: true }
---
You are in **Perceive** on the **bookings lane** — the mail ingest recorded a change (vetted org
data; the Sibyl entities are ALREADY written). Your job is the human side, nothing else.

1. New booking: is the guest's chat already bound (`memory_recall("guest", ...)` by any chat you
   can associate — usually it is NOT bound yet for a fresh booking)? Carry the deep link the
   payload brings.
2. Cancellation: was the guest in-house or pre-arrival? Anything already scheduled (cleaning
   notices) that must be recalled?
3. Transition to `bookings/express` with a precise gist: who to tell what. A modification with no
   human-facing consequence terminates quietly.

---resume---
(Resuming.) Entities already written; decide who hears what, carry forward.
