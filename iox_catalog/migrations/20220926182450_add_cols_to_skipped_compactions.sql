ALTER TABLE IF EXISTS skipped_compactions 
    ADD COLUMN IF NOT EXISTS num_files BIGINT DEFAULT NULL,
    ADD COLUMN IF NOT EXISTS limit_num_files BIGINT DEFAULT NULL,
    ADD COLUMN IF NOT EXISTS estimated_bytes BIGINT DEFAULT NULL,
    ADD COLUMN IF NOT EXISTS limit_bytes BIGINT DEFAULT NULL;