-- NULL follows the report's project. An empty override keeps the delivery unassigned.
ALTER TABLE ownership_card ADD COLUMN project_override TEXT;
