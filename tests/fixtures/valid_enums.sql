--- A commented declaration
CREATE TYPE public.access_management AS ENUM (
    'global',
    'contextual',
    'forbidden'
);

COMMENT ON TYPE public.access_management IS 'Represents access management levels.';

CREATE TYPE public.analytics_context AS ENUM (
    'admin',
    'frontend'
);

--- A declaration with no / default schema
CREATE TYPE asset_kind AS ENUM (
    'unknown',
    'image',
    'video',
    'audio',
    'pdf',
    'document',
    'archive'
);
