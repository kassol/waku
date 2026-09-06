CREATE TABLE `session_events` (
	`session_id` text NOT NULL,
	`runtime_id` text NOT NULL,
	`epoch` text NOT NULL,
	`sequence` integer NOT NULL,
	`data` text NOT NULL,
	PRIMARY KEY(`session_id`, `runtime_id`, `epoch`, `sequence`)
);
