;;; gitq-scrollback.el --- browse tmux scrollback as command entries  -*- lexical-binding: t; -*-

;; Author: baleti
;; Keywords: tools, terminals, convenience
;; Package-Requires: ((emacs "27.1") (gitq "0"))

;;; Commentary:

;; A companion to gitq.el for the scrollback subsystem (see
;; doc/scrollback.org).  It captures a tmux pane's scrollback via
;; `gitq --scrollback --sexp' and shows it in a buffer whose navigation is
;; *entry-based*: `j'/`k' jump whole command+output groups, mirroring the
;; vim-key navigation of the standalone `gitq --scrollback-browse' TUI,
;; rather than the buffer's raw lines.  This is the direct Emacs analogue
;; of how `gitq-results-mode' jumps whole hunks via a text property.
;;
;; Two entry points, one of which needs no shell integration at all:
;;   - `gitq-scrollback-open-from-file' — called by the zsh `\ee' widget
;;     via emacsclient, reading the sexp from a throwaway temp file.
;;   - `M-x gitq-scrollback' — Emacs runs gitq itself; useful from a
;;     `vterm'/`shell' buffer that is inside a tmux pane.

;;; Code:

(require 'gitq)
(require 'ansi-color)
(require 'subr-x)

(defvar gitq-scrollback-mode-map
  (let ((m (make-sparse-keymap)))
    (define-key m (kbd "j")   #'gitq-scrollback-next-entry)
    (define-key m (kbd "k")   #'gitq-scrollback-previous-entry)
    (define-key m (kbd "n")   #'gitq-scrollback-next-entry)
    (define-key m (kbd "p")   #'gitq-scrollback-previous-entry)
    (define-key m (kbd "h")   #'gitq-scrollback-fold-entry)
    (define-key m (kbd "l")   #'gitq-scrollback-unfold-entry)
    (define-key m (kbd "TAB") #'gitq-scrollback-toggle-fold)
    (define-key m (kbd "g g") #'gitq-scrollback-first-entry)
    (define-key m (kbd "G")   #'gitq-scrollback-last-entry)
    (define-key m (kbd "y")   #'gitq-scrollback-copy-command)
    (define-key m (kbd "q")   #'quit-window)
    m)
  "Keymap for `gitq-scrollback-mode'; j/k move by ENTRY, not by line.")

(define-derived-mode gitq-scrollback-mode special-mode "GitQ-Scrollback"
  "Major mode for browsing captured tmux scrollback.
Movement is entry-based: `j'/`k' jump between command/output groups,
mirroring the entry navigation in the standalone
`gitq --scrollback-browse' TUI rather than the buffer's raw lines."
  :interactive nil
  (setq truncate-lines t)
  (setq buffer-read-only t)
  (add-to-invisibility-spec '(gitq-scrollback-fold . t)))

;;; Rendering

(defun gitq-scrollback-open-from-file (file)
  "Read gitq scrollback sexp entries from FILE and open them in a buffer.
Deletes FILE afterward — it is a throwaway temp file written by the zsh
widget or the TUI's `e' action."
  (unwind-protect
      (with-temp-buffer
        (insert-file-contents file)
        (let ((entries (car (read-from-string (concat "(" (buffer-string) ")")))))
          (gitq-scrollback--display entries)))
    (ignore-errors (delete-file file))))

(defun gitq-scrollback--display (entries)
  "Render ENTRIES (a list of plists) into the scrollback buffer."
  (with-current-buffer (get-buffer-create "*gitq-scrollback*")
    (let ((inhibit-read-only t))
      (erase-buffer)
      (if (null entries)
          (insert "(no scrollback entries)\n")
        (dolist (e entries)
          (gitq-scrollback--insert-entry e)))
      (gitq-scrollback-mode)
      (goto-char (point-min)))
    (pop-to-buffer (current-buffer))))

(defun gitq-scrollback--insert-entry (e)
  "Insert one entry plist E, tagging its whole span with text properties.
The command header is always visible; the output region is recorded on
the entire entry span (as markers) so `h'/`l' can fold the output from
anywhere in the entry, header included."
  (let ((start (point))
        (code (plist-get e :exit-code)))
    (insert (propertize (format "[%d] " (plist-get e :index)) 'face 'shadow))
    (insert (or (plist-get e :command) "(no command)"))
    (when code
      (insert (propertize (format "  exit %d" code)
                          'face (if (zerop code) 'success 'error))))
    (insert "\n")
    (let* ((out-start (point))
           (out (or (plist-get e :output) "")))
      (insert out)
      ;; the CLI emits ANSI SGR in :output; let Emacs's own renderer map it
      (ansi-color-apply-on-region out-start (point))
      (unless (or (string-empty-p out) (string-suffix-p "\n" out))
        (insert "\n"))
      (let ((out-end (point)))
        ;; tag the whole entry span for entry-based navigation, and record
        ;; its output region (markers, so they track edits) over the same
        ;; span so folding is reachable from the header line too
        (put-text-property start out-end 'gitq-scrollback-entry e)
        (put-text-property start out-end 'gitq-scrollback-output
                           (cons (copy-marker out-start) (copy-marker out-end)))))))

;;; Entry-based navigation — the Emacs analogue of the TUI's j/k.
;;
;; Entries tile the buffer with distinct plist values under
;; `gitq-scrollback-entry', so a value change marks each boundary.  The
;; helper resolves the start of the entry containing a position, which
;; makes previous/last robust whether point sits mid-entry or exactly on a
;; boundary (where a naive double `previous-single-property-change' would
;; overshoot by one entry).

(defun gitq-scrollback--entry-start (pos)
  "Buffer position of the start of the entry containing POS."
  (if (or (<= pos (point-min))
          (not (eq (get-text-property pos 'gitq-scrollback-entry)
                   (get-text-property (1- pos) 'gitq-scrollback-entry))))
      pos
    (or (previous-single-property-change pos 'gitq-scrollback-entry)
        (point-min))))

(defun gitq-scrollback-next-entry ()
  "Move point to the start of the next entry (not the next line)."
  (interactive nil gitq-scrollback-mode)
  (let ((next (next-single-property-change (point) 'gitq-scrollback-entry)))
    (if next
        (goto-char next)
      (user-error "No next entry"))))

(defun gitq-scrollback-previous-entry ()
  "Move point to the start of the previous entry (not the previous line)."
  (interactive nil gitq-scrollback-mode)
  (let ((cur (gitq-scrollback--entry-start (point))))
    (if (> cur (point-min))
        (goto-char (gitq-scrollback--entry-start (1- cur)))
      (user-error "No previous entry"))))

(defun gitq-scrollback-first-entry ()
  "Jump to the first entry."
  (interactive nil gitq-scrollback-mode)
  (goto-char (point-min)))

(defun gitq-scrollback-last-entry ()
  "Jump to the last entry."
  (interactive nil gitq-scrollback-mode)
  (goto-char (gitq-scrollback--entry-start (max (point-min) (1- (point-max))))))

;;; Folding — reuse the overlay+invisibility approach from gitq-results.

(defun gitq-scrollback--output-region ()
  "The (BEG . END) output region for the entry at point, or nil."
  (let ((region (or (get-text-property (point) 'gitq-scrollback-output)
                    (get-text-property (line-beginning-position)
                                       'gitq-scrollback-output))))
    (when region
      (cons (marker-position (car region)) (marker-position (cdr region))))))

(defun gitq-scrollback--fold-overlay (beg end)
  "The fold overlay covering BEG..END, if any."
  (seq-find (lambda (ov) (overlay-get ov 'gitq-scrollback-fold))
            (overlays-in beg end)))

(defun gitq-scrollback-fold-entry ()
  "Collapse the selected entry's output (its header stays visible)."
  (interactive nil gitq-scrollback-mode)
  (let ((region (gitq-scrollback--output-region)))
    (unless region
      (user-error "No entry output at point"))
    (let ((beg (car region)) (end (cdr region)))
      (unless (or (= beg end) (gitq-scrollback--fold-overlay beg end))
        (let ((ov (make-overlay beg end)))
          (overlay-put ov 'gitq-scrollback-fold t)
          (overlay-put ov 'invisible 'gitq-scrollback-fold)
          (overlay-put ov 'isearch-open-invisible #'delete-overlay))))))

(defun gitq-scrollback-unfold-entry ()
  "Expand the selected entry's output."
  (interactive nil gitq-scrollback-mode)
  (let ((region (gitq-scrollback--output-region)))
    (unless region
      (user-error "No entry output at point"))
    (let ((ov (gitq-scrollback--fold-overlay (car region) (cdr region))))
      (when ov (delete-overlay ov)))))

(defun gitq-scrollback-toggle-fold ()
  "Toggle folding of the selected entry's output."
  (interactive nil gitq-scrollback-mode)
  (let ((region (gitq-scrollback--output-region)))
    (unless region
      (user-error "No entry output at point"))
    (if (gitq-scrollback--fold-overlay (car region) (cdr region))
        (gitq-scrollback-unfold-entry)
      (gitq-scrollback-fold-entry))))

;;; Actions

(defun gitq-scrollback-copy-command ()
  "Copy the selected entry's command to the kill ring."
  (interactive nil gitq-scrollback-mode)
  (let* ((e (get-text-property (point) 'gitq-scrollback-entry))
         (cmd (and e (plist-get e :command))))
    (if cmd
        (progn (kill-new cmd) (message "Copied: %s" cmd))
      (user-error "No command on this entry"))))

;;;###autoload
(defun gitq-scrollback (&optional tmux-target)
  "Capture the current (or TMUX-TARGET) tmux pane's scrollback and browse it.
Runs the gitq binary directly, so it needs no zsh integration — useful
from a `vterm' or `shell' buffer running inside a tmux pane."
  (interactive)
  (gitq--ensure-executable)
  (pcase-let ((`(,code . ,out)
               (if tmux-target
                   (gitq--run "--scrollback" "--sexp" "--tmux-target" tmux-target)
                 (gitq--run "--scrollback" "--sexp"))))
    (if (zerop code)
        (gitq-scrollback--display (car (read-from-string (concat "(" out ")"))))
      (user-error "%s" (string-trim out)))))

(provide 'gitq-scrollback)

;;; gitq-scrollback.el ends here
