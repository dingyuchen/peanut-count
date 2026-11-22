-- Ledgers created before Drizzle already have these tables, so this baseline
-- adopts them instead of failing. Later migrations are generated unchanged.
CREATE TABLE IF NOT EXISTS `cursors` (
	`scope` text PRIMARY KEY NOT NULL,
	`after` integer NOT NULL
);
--> statement-breakpoint
CREATE TABLE IF NOT EXISTS `preferences` (
	`key` text PRIMARY KEY NOT NULL,
	`value` text NOT NULL
);
--> statement-breakpoint
CREATE TABLE IF NOT EXISTS `statements` (
	`scope` text NOT NULL,
	`id` text NOT NULL,
	`body` text NOT NULL,
	`pending` integer NOT NULL,
	PRIMARY KEY(`scope`, `id`)
);
