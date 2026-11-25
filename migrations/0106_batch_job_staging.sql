-- Phase 7 (D-021): a Bedrock or Gemini job keeps the staging it was sent
-- through. A connection's staging may change or be deleted while its jobs
-- run; each job is created and collected through the bucket, prefix,
-- region and role it was given when it was first sent, never the
-- connection's current ones.
ALTER TABLE gateway.batch_job
    ADD COLUMN staging_bucket TEXT,
    ADD COLUMN staging_prefix TEXT,
    ADD COLUMN staging_region TEXT,
    ADD COLUMN staging_role_arn TEXT,
    ADD CONSTRAINT batch_job_staging_ck CHECK (
        (staging_bucket IS NULL AND staging_prefix IS NULL
             AND staging_region IS NULL AND staging_role_arn IS NULL)
        OR (staging_bucket IS NOT NULL AND staging_prefix IS NOT NULL));
