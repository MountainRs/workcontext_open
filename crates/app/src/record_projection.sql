visible_input AS (
    SELECT i.* FROM context_input i WHERE i.profile=?1
      AND (i.origin_ref IS NULL OR i.id IN (SELECT value FROM json_each($allowed)))
),
visible_facet AS (
    SELECT f.* FROM organizer_facet f JOIN visible_input i ON i.id=f.input_id AND i.source_version=f.version
    WHERE f.profile=?1
),
visible_correction AS (
    SELECT c.profile,c.record_id,c.version,c.scope_ids,missing,
        CASE WHEN c.content IS NOT NULL AND missing THEN '' ELSE c.content END AS content,
        CASE WHEN missing THEN NULL ELSE c.attribution END AS attribution
    FROM (SELECT c.*,EXISTS (SELECT 1 FROM json_each(c.evidence_versions) e WHERE NOT EXISTS (
        SELECT 1 FROM visible_input i WHERE i.id=e.key AND (
            (e.type='integer' AND i.source_version=e.value) OR
            (e.type='object' AND i.source_version=json_extract(e.value,'$.version') AND EXISTS (
                SELECT 1 FROM visible_facet f WHERE f.record_id=c.record_id AND f.input_id=i.id
                AND f.reply_ref=json_extract(e.value,'$.ref') AND f.reply_version=json_extract(e.value,'$.proof')))))) AS missing
        FROM current_record_correction c WHERE c.profile=?1) c
),
projected_record AS (
    SELECT r.id,r.profile,i.id AS input_id,
        CASE WHEN r.organizer_created THEN coalesce(primary_facet.title,nullif(substr(c.content,1,80),''),'已撤回的整理记录') ELSE r.title END AS title,
        coalesce(c.content,CASE WHEN r.organizer_created THEN coalesce(primary_facet.content,'') ELSE r.content END) AS content,
        coalesce(primary_facet.kind,r.kind) AS kind,
        CASE WHEN r.organizer_created AND i.id<>r.input_id THEN i.source_occurred_at ELSE r.occurred_at END AS occurred_at,
        r.created_at,r.reported_result,r.inference,r.open_questions,r.scope_hints,
        coalesce(c.attribution,CASE WHEN c.content IS NULL AND r.organizer_created THEN 'derived' ELSE 'reported' END) AS attribution,
        r.revision AS version,c.content AS corrected_content,c.attribution AS corrected_attribution,c.scope_ids AS corrected_scopes,
        CASE WHEN c.scope_ids IS NOT NULL THEN 'correction' WHEN EXISTS(SELECT 1 FROM record_scope s WHERE s.record_id=r.id)
             THEN 'source' ELSE 'organizer' END AS scope_basis,
        coalesce(i.origin_ref,i.id) AS source_ref,
        CASE WHEN i.origin_ref IS NOT NULL THEN 'archive' WHEN i.producer IS NOT NULL THEN 'agent' ELSE 'manual' END AS origin,
        r.organizer_created,i.producer,i.processing_state,
        (SELECT json_group_array(json_object('title',f.title,'scopeId',f.scope_id,'sourceRef',f.origin_ref,
            'version',f.version,'classification',json(f.facets_json)) ORDER BY f.input_id,f.slot)
          FROM visible_facet f WHERE f.record_id=r.id) AS derived_json
    FROM context_record r
    LEFT JOIN visible_facet primary_facet ON r.organizer_created=1 AND primary_facet.record_id=r.id AND (primary_facet.input_id,primary_facet.slot)=(
        SELECT f.input_id,f.slot FROM visible_facet f WHERE f.record_id=r.id ORDER BY f.input_id,f.slot LIMIT 1)
    JOIN visible_input i ON i.id=coalesce(primary_facet.input_id,r.input_id)
    LEFT JOIN visible_correction c ON c.profile=r.profile AND c.record_id=r.id
    WHERE r.profile=?1 AND (r.organizer_created=0 OR primary_facet.input_id IS NOT NULL OR (c.version IS NOT NULL AND NOT c.missing))
),
projected_scope AS (
    SELECT s.profile,s.record_id,s.scope_id FROM effective_record_scope s WHERE s.profile=?1
    UNION
    SELECT f.profile,f.record_id,f.scope_id FROM visible_facet f JOIN projected_record r ON r.id=f.record_id
    WHERE f.scope_id IS NOT NULL AND r.corrected_scopes IS NULL
      AND NOT EXISTS (SELECT 1 FROM record_scope s WHERE s.profile=f.profile AND s.record_id=f.record_id)
)
