-- Only accounts created after tracking was installed can have complete reach history.
CREATE TABLE eunha.account_reach (
    account_id bigint PRIMARY KEY REFERENCES public.accounts(id) ON DELETE CASCADE,
    inbox_hosts text[] DEFAULT ARRAY[]::text[],
    closed boolean NOT NULL DEFAULT false
);

CREATE FUNCTION eunha.initialize_account_reach() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO eunha.account_reach (account_id) VALUES (NEW.id);
    RETURN NEW;
END;
$$;

CREATE TRIGGER initialize_account_reach
AFTER INSERT ON public.accounts
FOR EACH ROW WHEN (NEW.domain IS NULL)
EXECUTE FUNCTION eunha.initialize_account_reach();
