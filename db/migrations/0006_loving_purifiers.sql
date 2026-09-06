CREATE TABLE `session_creations` (
	`id` text PRIMARY KEY NOT NULL,
	`manager_session_id` text NOT NULL,
	`idempotency_key` text,
	`data` text NOT NULL,
	`complete` integer NOT NULL
);
--> statement-breakpoint
CREATE UNIQUE INDEX `session_creations_by_manager_key` ON `session_creations` (`manager_session_id`,`idempotency_key`);--> statement-breakpoint
CREATE INDEX `session_creations_incomplete` ON `session_creations` (`complete`);