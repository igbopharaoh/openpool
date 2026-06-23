# PostgreSQL backup and restore drill

Run this drill monthly in technical staging and attach the timestamp, restore target, RPO, and RTO
to the release record.

1. Confirm automated backups and point-in-time recovery are enabled; record the latest recoverable timestamp.
2. Restore to a new isolated instance at a chosen point no more than the RPO behind current time.
3. Run the release image's migration-only task against the restored database. Never point a restore drill at production.
4. Start a disposable API and worker against the restored database, then verify `/readyz`, one public raffle, and one proof hash.
5. Compare row counts for raffles, invoices, entries, payouts, refunds, proofs, and audit events; investigate every mismatch.
6. Destroy the restored instance and revoke its temporary credentials. Record measured RTO and any remediation.
