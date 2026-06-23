# Release rollback runbook

1. Freeze deployment and record the immutable image digest, migration version, incident time, and affected raffle IDs.
2. Stop worker desired count first; this prevents new external payout attempts while state is assessed.
3. Roll API and worker task definitions back to the last healthy immutable digest. Do not roll back SQL migrations destructively.
4. If a migration is implicated, apply a reviewed forward compensating migration and restore only through the backup drill procedure.
5. Re-enable workers only after checking job leases, dead payout/refund jobs, canonical Bitcoin facts, and proof publication state.
6. Run the technical-staging failure drill and retain logs/metrics before closing the incident.
