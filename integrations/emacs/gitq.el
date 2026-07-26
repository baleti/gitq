;;; gitq.el --- Emacs front-end for the gitq CLI  -*- lexical-binding: t; -*-

;; Author: baleti
;; URL: https://github.com/baleti/gitq
;; Package-Requires: ((emacs "28.1"))
;; Version: 0.7.1

;;; Commentary:

;; Emacs front-end for gitq, the typed, categorical query language for
;; git.  All parsing, type checking, completion candidates, and execution
;; live in the `gitq' binary (the Rust implementation in this repo);
;; this package provides the interactive Emacs experience on top of it:
;;
;; - `M-x gitq': minibuffer entry with context-aware completion for the
;;   current pipeline position (candidates and their annotations come from
;;   `gitq --complete-annotated', so they can never drift from the parser),
;;   plus a debounced live preview of results while typing.
;; - a results buffer (`gitq-results-mode') with RET to visit the object
;;   at point (via magit when available), `b' to branch off, `c' to copy
;;   the SHA, `q' to quit.
;; - TAB in that buffer refines the query from the object under point:
;;   the minibuffer offers the morphisms and steps that type-check against
;;   that object's frame shape, and a complete move runs immediately, so
;;   repeated TAB walks down git's object graph one level per keystroke.
;; - re-invoking `gitq' from a results buffer pre-fills the minibuffer
;;   with the pipeline that produced it.
;;
;; Pipelines whose terminal is effectful (/branch-off, /commit, ...) run
;; through the CLI so the effect happens exactly as documented there;
;; /show and terminal-less pipelines render into the results buffer.

;;; Code:

(require 'cl-lib)

(defgroup gitq nil
  "Emacs front-end for the gitq CLI."
  :group 'tools)

(defcustom gitq-executable "gitq"
  "Path to the gitq binary."
  :type 'string :group 'gitq)

(defcustom gitq-preview-debounce 0.2
  "Seconds of no further input change before gitq (re)previews results."
  :type 'number :group 'gitq)

(defcustom gitq-preview-max-results 300
  "Maximum frames rendered in the live preview buffer.
Short in-progress patterns (\"grep wo\") can match thousands of lines;
the binary answers in well under 100ms, but redrawing a full frame of
them on every debounced keystroke is what makes typing feel laggy.  The
final run (RET) always renders everything."
  :type 'natnum :group 'gitq)

(defcustom gitq-match-face 'match
  "Face used to highlight query matches in the results buffer.
Defaults to the standard `match' face, the same one grep and occur
buffers use."
  :type 'face :group 'gitq)

(defcustom gitq-release-url "https://github.com/baleti/gitq/releases/latest/download/"
  "Base URL prebuilt gitq binaries are downloaded from.
Assets are published by the repo's release workflow (one per platform,
e.g. \"gitq-x86_64-linux\")."
  :type 'string :group 'gitq)

(defcustom gitq-install-directory (expand-file-name "~/.local/bin/")
  "Directory `gitq-install-binary' downloads the gitq binary into."
  :type 'directory :group 'gitq)

;;; Running the CLI

(defvar gitq--resolved-executable nil
  "Absolute path of the auto-downloaded binary, when one is in use.
Nil while `gitq-executable' resolves on `exec-path' — a binary the user
installed themselves always wins over the auto-managed one.")

(defun gitq--executable ()
  "The gitq binary to invoke."
  (or gitq--resolved-executable gitq-executable))

(defun gitq--release-asset ()
  "Release asset name for this platform, or nil if none is published."
  (pcase (list system-type (car (split-string system-configuration "-")))
    (`(gnu/linux "x86_64") "gitq-x86_64-linux")
    (_ nil)))

(defun gitq-install-binary ()
  "Download the prebuilt gitq binary from the latest GitHub release.
Installs into `gitq-install-directory' and returns the binary's path.
The assets are built by the repo's release workflow; platforms without
a published asset must build from source (make install-native)."
  (interactive)
  (let ((asset (gitq--release-asset)))
    (unless asset
      (user-error "gitq: no prebuilt binary for %s/%s — build from source: cd <gitq repo> && make install-native"
                  system-type system-configuration))
    (let ((url (concat gitq-release-url asset))
          (dest (expand-file-name "gitq" gitq-install-directory)))
      (make-directory gitq-install-directory t)
      (message "gitq: downloading %s ..." url)
      (condition-case err
          (url-copy-file url dest t)
        (error
         (user-error "gitq: download from %s failed (%s) — build from source instead: make install-native"
                     url (error-message-string err))))
      (set-file-modes dest #o755)
      (message "gitq: installed %s" dest)
      dest)))

(defun gitq--ensure-executable ()
  "Ensure a gitq binary is available, fetching a release automatically.
Resolution order: `gitq-executable' on `exec-path' (a user-installed
binary always wins), then a previously downloaded binary in
`gitq-install-directory', then a fresh automatic download of the
prebuilt release asset — no interaction needed.  Only errors on
platforms with no published asset and no binary."
  (cond
   ((executable-find gitq-executable)
    (setq gitq--resolved-executable nil))
   ((let ((local (expand-file-name "gitq" gitq-install-directory)))
      (when (file-executable-p local)
        (setq gitq--resolved-executable local))))
   ((gitq--release-asset)
    (setq gitq--resolved-executable (gitq-install-binary)))
   (t
    (user-error "gitq: binary '%s' not found on `exec-path' and no prebuilt release exists for %s/%s — build from source (make install-native in the gitq repo) or set `gitq-executable'"
                gitq-executable system-type system-configuration))))

(defun gitq--run (&rest args)
  "Run the gitq binary with ARGS; return (EXIT-CODE . OUTPUT)."
  (with-temp-buffer
    (let ((code (apply #'call-process (gitq--executable) nil (list t nil) nil args)))
      (cons code (buffer-string)))))

(defun gitq--try-frames (pipeline)
  "Execute PIPELINE's source and steps (never its terminal).
Returns (:ok . FRAMES) — plists as printed by `gitq --sexp' — or
\(:error . MESSAGE) with the CLI's own message.  Callers that want to
treat \"does not parse yet\" as an answer rather than a failure (the
live preview, the refinement in `gitq-results-refine') use this
directly; `gitq--frames' is the signalling wrapper."
  (pcase-let ((`(,code . ,out) (gitq--run "--sexp" "--preview" pipeline)))
    (if (zerop code)
        (cons :ok (car (read-from-string (concat "(" out ")"))))
      (cons :error (string-trim out)))))

(defun gitq--frames (pipeline)
  "Execute PIPELINE's source and steps (never its terminal); return frames.
Signals a `user-error' with the CLI's message on failure."
  (pcase (gitq--try-frames pipeline)
    (`(:ok . ,frames) frames)
    (`(,_ . ,msg)     (user-error "%s" msg))))

;;; Results buffer

(defvar gitq-results-mode-map
  (let ((m (make-sparse-keymap)))
    (define-key m (kbd "RET") #'gitq-results-visit)
    (define-key m (kbd "b")   #'gitq-results-branch-off)
    (define-key m (kbd "c")   #'gitq-results-copy-sha)
    (define-key m (kbd "q")   #'quit-window)
    ;; TAB continues the query from the object at point; `.' is its
    ;; fallback for setups where evil owns TAB (in terminals TAB = C-i).
    (define-key m (kbd "TAB")       #'gitq-results-refine)
    (define-key m (kbd ".")         #'gitq-results-refine)
    ;; folding: `f' toggles the hunk at point, S-TAB / `F' the whole buffer
    (define-key m (kbd "<backtab>") #'gitq-results-toggle-fold-all)
    (define-key m (kbd "f")         #'gitq-results-toggle-fold)
    (define-key m (kbd "F")         #'gitq-results-toggle-fold-all)
    m)
  "Keymap for `gitq-results-mode'.")

(define-derived-mode gitq-results-mode special-mode "GitQ"
  "Major mode for displaying gitq pipeline results."
  :interactive nil
  (setq truncate-lines t)
  (add-to-invisibility-spec '(gitq-fold . t)))

(defvar-local gitq--buffer-pipeline nil
  "The pipeline string that produced this `gitq-results-mode' buffer.
Set by `gitq--render' after the major mode is turned on (major modes
call `kill-all-local-variables', which would otherwise wipe it).")

(defconst gitq--token-regexp "\"[^\"]*\"\\|/[^/ ]*/\\|[^ \t]+"
  "Regexp matching one pipeline token, quoted values and /regex/ first.
A rough client-side echo of the binary's tokenizer, used only where
Emacs needs to look at the shape of a pipeline string it already has
\(highlighting its search terms, finding its terminal) — never to decide
what is valid, which is always the binary's answer.")

(defun gitq--terminal-token-p (token)
  "Return non-nil if TOKEN is a /terminal rather than a value.
Terminal names are lowercase words with hyphens, so an unquoted
/regex/ literal or an absolute path glob is not mistaken for one."
  (and (stringp token) (string-match-p "\\`/[a-z][a-z-]*\\'" token)))

(defun gitq--pipeline-without-terminal (pipeline)
  "PIPELINE truncated before its terminal, if it has one.
A terminal ends a pipeline, so it has to go before anything can be
appended to the query that produced a results buffer."
  (let ((start 0) (cut nil))
    (while (and (null cut) (string-match gitq--token-regexp (or pipeline "") start))
      (setq start (match-end 0))
      (when (gitq--terminal-token-p (match-string 0 pipeline))
        (setq cut (match-beginning 0))))
    (string-trim (if cut (substring pipeline 0 cut) (or pipeline "")))))

(defun gitq--face (magit-face fallback)
  "Return MAGIT-FACE if it is defined, else FALLBACK."
  (if (facep magit-face) magit-face fallback))

(defun gitq--highlight-regexps (pipeline)
  "Extract regexps worth highlighting in results from PIPELINE.
Covers the text the user is searching for: `grep'/`pickaxe' patterns
and `where' values on the content and message fields.  Quoted and bare
values are matched literally; /regex/ literals as regexps."
  (let ((tokens nil) (start 0) res)
    (while (string-match gitq--token-regexp pipeline start)
      (push (match-string 0 pipeline) tokens)
      (setq start (match-end 0)))
    (setq tokens (nreverse tokens))
    (while tokens
      (let ((tok (pop tokens)))
        (cond
         ((member tok '("grep" "pickaxe"))
          (when tokens (push (gitq--term-to-regexp (pop tokens)) res)))
         ((member tok '("content" "message"))
          (let ((next (car tokens)))
            (cond
             ((member next '("==" "!=" "regex"))
              (pop tokens)
              (when tokens (push (gitq--term-to-regexp (pop tokens)) res)))
             ((and next (not (member next '("via" "where" "grep" "pickaxe" "path" "pick"
                                            "sort" ","))))
              (push (gitq--term-to-regexp (pop tokens)) res))))))))
    (delete-dups (delq nil (nreverse res)))))

(defun gitq--term-to-regexp (tok)
  "Convert a pipeline value token to a highlight regexp, or nil."
  (cond
   ((null tok) nil)
   ((string-match "\\`\"\\(.*\\)\"\\'" tok) (regexp-quote (match-string 1 tok)))
   ((string-match "\\`/\\(.*\\)/\\'" tok) (match-string 1 tok))
   (t (regexp-quote tok))))

(defvar gitq--active-highlights nil
  "Regexps highlighted while rendering the current results, or nil.")

(defun gitq--apply-highlights (start end)
  "Add `gitq-match-face' to matches of the active regexps in START..END."
  (dolist (re gitq--active-highlights)
    (save-excursion
      (goto-char start)
      (ignore-errors
        (while (re-search-forward re end t)
          (add-face-text-property (match-beginning 0) (match-end 0)
                                  gitq-match-face))))))

(defun gitq--insert-commit-header (frame)
  "Insert a commit metadata header line for a hunk/diff-line FRAME."
  (insert (propertize (substring (plist-get frame :commit-sha) 0 8)
                      'face (gitq--face 'magit-hash 'shadow)))
  (when-let ((author (plist-get frame :author)))
    (insert "  ")
    (insert (propertize author
                        'face (gitq--face 'magit-log-author 'font-lock-string-face))))
  (when-let ((date (plist-get frame :date)))
    (insert "  ")
    (insert (propertize (substring date 0 (min 10 (length date)))
                        'face (gitq--face 'magit-log-date 'shadow))))
  (when-let ((msg (plist-get frame :message)))
    (insert "  ") (insert msg))
  (put-text-property (line-beginning-position) (point)
                     'gitq-sha (plist-get frame :commit-sha))
  (insert "\n"))

(defun gitq--frame-commit-sha (frame)
  "Return the commit SHA for FRAME (direct or via :commit-sha)."
  (or (plist-get frame :commit-sha)
      (plist-get frame :sha)))

(defun gitq--insert-frame (frame)
  "Insert a human-readable line for FRAME into the current buffer."
  (let ((type  (plist-get frame :type))
        (start (point)))
    (pcase type
      ('commit
       (let* ((sha   (plist-get frame :sha))
              (short (when sha (substring sha 0 (min 8 (length sha))))))
         (insert (propertize (or short "?")
                             'face (gitq--face 'magit-hash 'shadow)))
         (insert "  ")
         (let ((author (plist-get frame :author)))
           (when author
             (insert (propertize
                      (format "%-20s"
                              (substring author 0 (min 20 (length author))))
                      'face (gitq--face 'magit-log-author 'font-lock-string-face)))))
         (let ((date (plist-get frame :date)))
           (when date
             (insert (propertize (substring date 0 (min 10 (length date)))
                                 'face (gitq--face 'magit-log-date 'shadow)))
             (insert "  ")))
         (insert (or (plist-get frame :message) ""))))
      ('blob
       (insert (propertize (or (plist-get frame :path) "?")
                           'face (gitq--face 'magit-filename 'font-lock-function-name-face))))
      ('ref
       (insert (propertize (or (plist-get frame :name) "?")
                           'face (gitq--face 'magit-branch-local 'font-lock-keyword-face)))
       (when-let ((sha (plist-get frame :sha)))
         (insert "  ")
         (insert (propertize (substring sha 0 (min 8 (length sha)))
                             'face (gitq--face 'magit-hash 'shadow)))))
      ('worktree
       (insert (propertize (or (plist-get frame :path) "?")
                           'face (gitq--face 'magit-filename 'font-lock-function-name-face)))
       (when-let ((b (plist-get frame :branch)))
         (insert "  ")
         (insert (propertize b 'face (gitq--face 'magit-branch-local 'font-lock-keyword-face)))))
      ('line
       (insert (propertize (or (plist-get frame :path) "?")
                           'face (gitq--face 'magit-filename 'font-lock-function-name-face)))
       (insert ":")
       (insert (propertize (number-to-string (or (plist-get frame :line-number) 0))
                           'face 'shadow))
       (insert ": ")
       (insert (or (plist-get frame :content) "")))
      ('hunk
       (insert (propertize (or (plist-get frame :path) "?")
                           'face (gitq--face 'magit-filename 'font-lock-function-name-face)))
       (insert (propertize (format " lines %d-%d"
                                   (or (plist-get frame :start-line) 0)
                                   (or (plist-get frame :end-line) 0))
                           'face 'shadow))
       (let ((body-start (point)))
         (when-let ((content (plist-get frame :content)))
           (dolist (line (split-string (string-trim-right content "\n+") "\n"))
             (insert "\n")
             (insert (propertize line 'face
                                 (pcase (and (> (length line) 0) (aref line 0))
                                   (?+ 'diff-added)
                                   (?- 'diff-removed)
                                   (_  'default))))))
         ;; foldable body: TAB anywhere on the hunk toggles it (the
         ;; property spans the whole hunk so point can be on any line)
         (when (> (point) body-start)
           (put-text-property start (point) 'gitq-fold-body
                              (cons (copy-marker body-start)
                                    (copy-marker (point)))))))
      ('diff-line
       (let* ((sign  (or (plist-get frame :sign) "?"))
              (added (equal sign "+")))
         ;; the grouped commit header carries the hash/author/date
         (insert "  ")
         (insert (propertize (or (plist-get frame :path) "?")
                             'face (gitq--face 'magit-filename 'font-lock-function-name-face)))
         (insert ":")
         (insert (propertize (number-to-string (or (plist-get frame :line-number) 0))
                             'face 'shadow))
         (insert ": ")
         (insert (propertize (concat sign (or (plist-get frame :content) ""))
                             'face (if added 'diff-added 'diff-removed)))))
      (_
       ;; projected or unknown — dump key:value pairs
       (cl-loop for (k v) on frame by #'cddr
                do (insert (format "%s:%s " k v)))))
    (put-text-property start (point) 'gitq-frame frame)
    (put-text-property start (point) 'gitq-sha (gitq--frame-commit-sha frame))
    (insert "\n")))

(defun gitq--render (frames pipeline-str &optional truncated)
  "Render FRAMES into the *gitq* results buffer and return that buffer.
Hunk and diff-line frames are grouped under commit metadata headers
(their frames carry the owning commit's author/date/message).  Matches
of the query's search terms are highlighted with `gitq-match-face'.
TRUNCATED, if non-nil, is how many further results were omitted (used
by the live preview)."
  (with-current-buffer (get-buffer-create "*gitq*")
    (let ((inhibit-read-only t)
          (gitq--active-highlights (gitq--highlight-regexps pipeline-str))
          (last-group-sha nil))
      (erase-buffer)
      (insert (propertize (format "gitq: %s\n\n" pipeline-str)
                          'face 'font-lock-comment-face))
      (if frames
          (dolist (f frames)
            ;; commit header when a hunk/diff-line group changes commit
            (when (and (memq (plist-get f :type) '(hunk diff-line line))
                       (plist-get f :commit-sha)
                       (not (equal (plist-get f :commit-sha) last-group-sha)))
              (unless (bobp) (insert "\n"))
              (gitq--insert-commit-header f)
              (setq last-group-sha (plist-get f :commit-sha)))
            (let ((start (point)))
              (gitq--insert-frame f)
              (gitq--apply-highlights start (point))))
        (insert "(no results)\n"))
      (when (and truncated (> truncated 0))
        (insert (propertize (format "\n... %d more results (RET on the query runs it in full)\n"
                                    truncated)
                            'face 'font-lock-comment-face)))
      (gitq-results-mode)
      (setq gitq--buffer-pipeline pipeline-str)
      (goto-char (point-min)))
    (current-buffer)))

(defun gitq--display (frames pipeline-str)
  "Show FRAMES in the *gitq* buffer, taking over the whole frame."
  (pop-to-buffer (gitq--render frames pipeline-str) '(display-buffer-full-frame)))

(defun gitq--preview-display (frames pipeline-str &optional truncated)
  "Show FRAMES in the *gitq* buffer without selecting its window.
`display-buffer-full-frame' works via `delete-other-windows', which
selects its target window as a side effect — during a minibuffer read
that silently steals focus from typing, so the previously selected
window is restored explicitly.  TRUNCATED is passed to `gitq--render'
(the count of preview results omitted beyond `gitq-preview-max-results')."
  (let ((previously-selected (selected-window)))
    (display-buffer (gitq--render frames pipeline-str truncated)
                    '(display-buffer-full-frame))
    (when (window-live-p previously-selected)
      (select-window previously-selected))))

(defun gitq--preview-message (pipeline-str msg)
  "Show MSG in the *gitq* buffer as the preview of PIPELINE-STR.
Used while a pipeline is incomplete: the parser's own explanation says
what is still missing, which is more useful than a stale result set —
and unlike a stale set it cannot be misread as this query's answer.
Matches what the terminal completion preview shows."
  (let ((previously-selected (selected-window)))
    (display-buffer
     (with-current-buffer (get-buffer-create "*gitq*")
       (let ((inhibit-read-only t))
         (erase-buffer)
         (insert (propertize (format "gitq: %s\n\n" pipeline-str)
                             'face 'font-lock-comment-face))
         (insert (propertize msg 'face 'error))
         (goto-char (point-min)))
       (current-buffer))
     '(display-buffer-full-frame))
    (when (window-live-p previously-selected)
      (select-window previously-selected))))

(defun gitq-results-visit ()
  "Visit the git object at point in the *gitq* buffer."
  (interactive nil gitq-results-mode)
  (let* ((frame (get-text-property (point) 'gitq-frame))
         (sha   (get-text-property (point) 'gitq-sha))
         (type  (plist-get frame :type)))
    (pcase type
      ('blob (when (and sha (fboundp 'magit-find-file))
               (magit-find-file sha (plist-get frame :path))))
      (_     (when (and sha (fboundp 'magit-show-commit))
               (magit-show-commit sha))))))

(defun gitq-results-branch-off ()
  "Create a branch from the commit at point in the *gitq* buffer."
  (interactive nil gitq-results-mode)
  (let ((sha (get-text-property (point) 'gitq-sha)))
    (unless sha (user-error "No commit at point"))
    (let ((name (read-string "Branch name: ")))
      (call-process "git" nil nil nil "checkout" "-b" name sha)
      (when (fboundp 'magit-refresh) (magit-refresh))
      (message "gitq: created branch '%s'" name))))

(defun gitq--fold-overlay-at (beg end)
  "The fold overlay covering BEG..END, if any."
  (seq-find (lambda (ov) (overlay-get ov 'gitq-fold))
            (overlays-in beg end)))

(defun gitq-results-toggle-fold ()
  "Fold or unfold the hunk body at point (header line stays visible)."
  (interactive nil gitq-results-mode)
  (let ((region (or (get-text-property (point) 'gitq-fold-body)
                    (get-text-property (line-beginning-position) 'gitq-fold-body))))
    (unless region
      (user-error "No foldable hunk at point"))
    (let* ((beg (marker-position (car region)))
           (end (marker-position (cdr region)))
           (existing (gitq--fold-overlay-at beg end)))
      (if existing
          (delete-overlay existing)
        (let ((ov (make-overlay beg end)))
          (overlay-put ov 'gitq-fold t)
          (overlay-put ov 'invisible 'gitq-fold)
          (overlay-put ov 'isearch-open-invisible #'delete-overlay))))))

(defun gitq--all-fold-regions ()
  "All distinct hunk body regions in the buffer, as (BEG . END)."
  (let (regions (pos (point-min)) region)
    (while (setq pos (next-single-property-change pos 'gitq-fold-body))
      (when (setq region (get-text-property pos 'gitq-fold-body))
        (cl-pushnew (cons (marker-position (car region))
                          (marker-position (cdr region)))
                    regions :test #'equal)))
    (nreverse regions)))

(defun gitq-results-toggle-fold-all ()
  "Fold every hunk body in the buffer, or unfold them all if any are folded."
  (interactive nil gitq-results-mode)
  (let ((folded (seq-filter (lambda (ov) (overlay-get ov 'gitq-fold))
                            (overlays-in (point-min) (point-max)))))
    (if folded
        (mapc #'delete-overlay folded)
      (dolist (region (gitq--all-fold-regions))
        (let ((ov (make-overlay (car region) (cdr region))))
          (overlay-put ov 'gitq-fold t)
          (overlay-put ov 'invisible 'gitq-fold)
          (overlay-put ov 'isearch-open-invisible #'delete-overlay))))))

(defun gitq-results-copy-sha ()
  "Copy the SHA at point to the kill ring."
  (interactive nil gitq-results-mode)
  (let ((sha (get-text-property (point) 'gitq-sha)))
    (if sha
        (progn (kill-new sha)
               (message "gitq: copied %s" (substring sha 0 (min 8 (length sha)))))
      (user-error "No SHA at point"))))

;;; Completion

(defun gitq--complete-candidates (input)
  "Return (CANDIDATE . DESCRIPTION) pairs for the pipeline string INPUT.
Delegates to `gitq --complete-annotated', the same engine the strict
parser is built on, so completion can never offer a token the parser
rejects.  Never signals — completion runs on every keystroke inside the
completion UI, so any failure (binary missing, not a repo) just means
no candidates; `gitq--ensure-executable' reports the actionable error
once, at the prompt."
  (condition-case nil
      (pcase-let ((`(,code . ,out) (gitq--run "--complete-annotated" input)))
        (when (zerop code)
          (mapcar (lambda (line)
                    (pcase-let ((`(,cand ,kind ,desc) (split-string line "\t")))
                      (list cand (or kind "") (or desc ""))))
                  (split-string out "\n" t))))
    (error nil)))

(defun gitq--current-token (string)
  "Return the in-progress partial token at the end of STRING, or \"\"."
  (if (string-match "\\(?:^\\|[ \t]\\)\\([^ \t]*\\)$" string)
      (match-string 1 string)
    ""))

(defvar gitq--completion-cache nil
  "Cons of (INPUT . CANDIDATES) memoizing the last CLI completion call.")

(defvar gitq--annotations nil
  "Alist of (CANDIDATE KIND DESCRIPTION) for the completion in progress.
Both annotators read it, so any gitq prompt — the growing pipeline, the
refinement moves in `gitq-results-refine' — annotates from whatever
table it built, without either of them owning the other's cache.")

(defun gitq--candidates-for (string)
  "Candidates (with descriptions) for STRING, memoized per input."
  (unless (equal (car gitq--completion-cache) string)
    (setq gitq--completion-cache
          (cons string (gitq--complete-candidates string))))
  (setq gitq--annotations (cdr gitq--completion-cache)))

(defun gitq--affixate (candidates)
  "Return CANDIDATES as (CAND \"\" DESC) triples for completion UIs.
The plain fallback used when Marginalia isn't around (see
`gitq--marginalia-annotate' for the Marginalia version)."
  (let ((table gitq--annotations))
    (mapcar (lambda (c)
              (let ((desc (nth 2 (assoc c table))))
                (list c ""
                      (if (and desc (not (string-empty-p desc)))
                          (propertize (concat "  " desc)
                                      'face 'completions-annotations)
                        ""))))
            candidates)))

(defun gitq--marginalia-annotate (cand)
  "Marginalia annotator for gitq completion candidate CAND.
Registered under the `gitq-token' category so Marginalia users get its
column alignment and faces; everyone else gets the plain
`gitq--affixate' fallback.  Deliberately built from plain runtime
`propertize'/`format' calls reading only the public
`marginalia-separator' variable and the `marginalia--align' text
property protocol — the `marginalia--fields' macros are invisible when
this file is byte-compiled without Marginalia loaded (straight.el
compiles packages in isolation), which would compile the field specs
into calls to nonexistent functions."
  (pcase-let ((`(,_ ,kind ,desc) (assoc cand gitq--annotations)))
    (when (or kind desc)
      (concat
       (propertize " " 'marginalia--align t)
       (if (boundp 'marginalia-separator) marginalia-separator "  ")
       (propertize (format "%-9s" (or kind "")) 'face 'marginalia-type)
       (if (boundp 'marginalia-separator) marginalia-separator "  ")
       (propertize (or desc "") 'face 'marginalia-documentation)))))

(defvar marginalia-annotator-registry)
(defvar marginalia-annotators)
(defvar marginalia-separator)

(with-eval-after-load 'marginalia
  (if (boundp 'marginalia-annotator-registry)
      (cl-pushnew '(gitq-token gitq--marginalia-annotate builtin none)
                  marginalia-annotator-registry :test #'equal)
    (cl-pushnew '(gitq-token gitq--marginalia-annotate builtin none)
                marginalia-annotators :test #'equal)))

(defun gitq--completion-table (string predicate action)
  "Dynamic `completing-read' collection table for a growing gitq pipeline.
Only the in-progress final token of STRING is completed; earlier tokens
are fixed context."
  (cond
   ((eq action 'metadata)
    '(metadata (category . gitq-token)
               (affixation-function . gitq--affixate)))
   ((eq (car-safe action) 'boundaries)
    (cons 'boundaries
          (cons (- (length string) (length (gitq--current-token string))) 0)))
   (t
    (complete-with-action action
                          (mapcar #'car (gitq--candidates-for string))
                          (gitq--current-token string)
                          predicate))))

;;; Reading a pipeline, with live preview

(defvar gitq--history nil "Minibuffer history list for `gitq'.")

(defvar gitq--history-locations (make-hash-table :test #'equal)
  "Pipeline string -> repo it was last run from (`gitq--location').
Feeds the location annotations and the repo-scoping toggle in
`gitq--history-search'.")

(defun gitq--location (&optional dir)
  "The repo root containing DIR (default `default-directory'), abbreviated."
  (abbreviate-file-name
   (or (locate-dominating-file (or dir default-directory) ".git")
       (or dir default-directory))))

(defun gitq--history-annotate (cand)
  "Annotate history entry CAND with the repo it was run from."
  (when-let ((loc (gethash cand gitq--history-locations)))
    (propertize (concat "  " loc)
                'face (if (facep 'marginalia-documentation)
                          'marginalia-documentation
                        'completions-annotations))))

(defun gitq--history-table (cands)
  "Completion table over history CANDS, newest first, location-annotated."
  (lambda (string pred action)
    (if (eq action 'metadata)
        '(metadata (category . gitq-history)
                   (display-sort-function . identity)
                   (cycle-sort-function . identity)
                   (annotation-function . gitq--history-annotate))
      (complete-with-action action cands string pred))))

(defvar gitq--history-map
  (let ((map (make-sparse-keymap)))
    (define-key map (kbd "M-.") (lambda () (interactive)
                                  (throw 'gitq--rescope t)))
    map)
  "Extra keys inside the history search: \\`M-.' toggles repo scoping.")

(defun gitq--minibuffer-selection ()
  "The candidate currently highlighted in this minibuffer.
Vertico's selection when Vertico is active (so moving through the list
previews each entry), otherwise the literal minibuffer contents."
  (if (and (bound-and-true-p vertico--input) (fboundp 'vertico--candidate))
      (vertico--candidate)
    (minibuffer-contents-no-properties)))

(defun gitq--attach-preview (get-input)
  "Attach the debounced live results preview to the current minibuffer.
GET-INPUT is called (in the minibuffer) to obtain the pipeline string
to preview.  Used by both the gitq prompt (previewing what's typed) and
the history search (previewing the highlighted entry)."
  (let ((mb-buffer (current-buffer))
        (last-input nil)
        (timer nil))
    (add-hook 'post-command-hook
              (lambda ()
                (when timer (cancel-timer timer))
                (setq timer
                      (run-with-timer
                       gitq-preview-debounce nil
                       (lambda ()
                         (when (buffer-live-p mb-buffer)
                           (with-current-buffer mb-buffer
                             (let ((input (funcall get-input)))
                               (unless (equal input last-input)
                                 (setq last-input input)
                                 (pcase (gitq--try-frames input)
                                   (`(:ok . ,frames)
                                    (let ((total (length frames)))
                                      (gitq--preview-display
                                       (seq-take frames gitq-preview-max-results)
                                       input
                                       (max 0 (- total gitq-preview-max-results)))))
                                   ;; A pipeline that does not parse yet used
                                   ;; to leave the previous preview on screen,
                                   ;; which reads as "this query produced
                                   ;; that" — the one thing a live preview
                                   ;; must never imply.  Show the CLI's own
                                   ;; message instead; it says what is still
                                   ;; missing.
                                   (`(:error . ,msg)
                                    (gitq--preview-message input msg)))))))))))
              nil t)))

(defun gitq--history-search ()
  "Pick a previous pipeline and run it immediately.
Selecting an entry replaces the minibuffer contents and submits it, so
the query executes and its results land in the *gitq* buffer right
away.  \\`M-.' toggles between all history and only the queries last
run from the current repo (each entry is annotated with its repo)."
  (interactive)
  (unless gitq--history
    (user-error "No gitq history yet"))
  (let* ((enable-recursive-minibuffers t)
         (here (gitq--location))
         (scoped nil)
         choice)
    (while (progn
             (setq choice
                   (catch 'gitq--rescope
                     (let ((cands (if scoped
                                      (seq-filter
                                       (lambda (q)
                                         (equal (gethash q gitq--history-locations) here))
                                       gitq--history)
                                    gitq--history)))
                       (unless cands
                         (user-error "No gitq history from %s yet" here))
                       (minibuffer-with-setup-hook
                           (lambda ()
                             (use-local-map (make-composed-keymap
                                             gitq--history-map (current-local-map)))
                             (gitq--attach-preview #'gitq--minibuffer-selection))
                         (completing-read
                          (format "gitq history%s (M-. → %s): "
                                  (if scoped (format " [%s]" here) "")
                                  (if scoped "all repos" "this repo only"))
                          (gitq--history-table cands) nil t)))))
             (eq choice t))
      (setq scoped (not scoped)))
    (delete-minibuffer-contents)
    (insert choice)
    ;; submit the outer gitq prompt: run the query for real
    (exit-minibuffer)))

(defvar gitq--pipeline-map
  (let ((map (make-sparse-keymap)))
    (define-key map (kbd "C-r") #'gitq--history-search)
    map)
  "Keymap merged into the minibuffer's local map by `gitq--read-pipeline'.
Window switching into the results buffer is deliberately not bound
here: rebind your usual window keys for minibuffers instead, e.g.
  (map! :map (minibuffer-local-map vertico-map) \"C-w\" evil-window-map)
\\`C-M-v' scrolls the results window without leaving the minibuffer.")

(defun gitq--preview-frames (input)
  "Return (:ok . FRAMES) if INPUT previews cleanly, else nil.
Runs the CLI with --preview (a terminal keyword ends the pipeline but
its action is never applied).  Returns nil, with no side effects, if
INPUT does not currently parse or execute cleanly."
  (ignore-errors
    (pcase (gitq--try-frames input)
      ((and result `(:ok . ,_)) result))))

(defun gitq--read-pipeline (prompt &optional initial-input)
  "Read a gitq pipeline with completion and a debounced live preview.
INITIAL-INPUT, if non-nil, pre-fills the minibuffer.  \\`C-r' opens a
history search over previously-run pipelines."
  (minibuffer-with-setup-hook
      (lambda ()
        (use-local-map (make-composed-keymap gitq--pipeline-map (current-local-map)))
        (gitq--attach-preview #'minibuffer-contents-no-properties))
    (completing-read prompt #'gitq--completion-table nil nil
                     initial-input 'gitq--history)))

;;; Refining from the results buffer

(defun gitq--quote-value (value)
  "VALUE as a where-condition literal: numbers bare, strings quoted."
  (if (numberp value) (number-to-string value) (format "%S" value)))

(defun gitq--identity-clause (frame specs)
  "A `where' clause pinning FRAME, from SPECS of (PROPERTY . FIELD).
Only properties FRAME actually carries are used, and every condition is
an equality on a field of that frame's own shape — so the clause always
type-checks against the frame it was built from."
  (let (conds)
    (dolist (spec specs)
      (when-let ((value (plist-get frame (car spec))))
        (push (format "%s == %s" (cdr spec) (gitq--quote-value value)) conds)))
    (when conds
      (concat "where " (mapconcat #'identity (nreverse conds) ", ")))))

(defun gitq--pinned (base frame specs)
  "BASE narrowed to FRAME by SPECS, or nil if there is no BASE to narrow."
  (let ((clause (and specs (gitq--identity-clause frame specs))))
    (cond ((or (null base) (string-empty-p base)) nil)
          (clause (concat base " " clause))
          (t base))))

(defun gitq--derived-anchor (frame morphism specs base)
  "Re-derive FRAME from its own commit through MORPHISM, pinned by SPECS.
Falls back to pinning inside BASE for a frame with no owning commit."
  (if-let ((sha (plist-get frame :commit-sha)))
      (let ((clause (gitq--identity-clause frame specs)))
        (concat sha " via " morphism (if clause (concat " " clause) "")))
    (gitq--pinned base frame specs)))

(defun gitq--frame-anchor (frame pipeline)
  "A pipeline whose result is just FRAME, to continue the query from.
PIPELINE is the query that produced the results buffer.

The frame under point decides the anchor, and through it which
morphisms the binary will accept next — so an anchor has to preserve
the frame's /shape/, not only its identity:

- a commit or a ref anchors as itself: a bare SHA or ref name is a
  source in its own right (\"abc123 via diff.hunks\"), and the cheapest
  one there is — nothing upstream is re-run.
- a hunk or a diff line is re-derived from its own commit through the
  morphism that gave it its shape (hunks come from `diff.hunks', diff
  lines from `diff.lines'), then pinned by path and line.  That keeps
  the hunk shape — so `via commit' and `via history' stay available —
  and still avoids re-running everything upstream.
- anything else (blobs, grep lines, worktrees, projections) has no
  standalone source form, so it is pinned inside the query it came
  from, with that query's terminal dropped."
  (let ((base (gitq--pipeline-without-terminal pipeline)))
    (pcase (plist-get frame :type)
      ('commit    (plist-get frame :sha))
      ('ref       (or (plist-get frame :name) (plist-get frame :sha)))
      ('hunk      (gitq--derived-anchor
                   frame "diff.hunks"
                   '((:path . "path") (:start-line . "start-line")) base))
      ;; a removed and an added line share a path and a line number, so
      ;; the sign is part of a diff line's identity
      ('diff-line (gitq--derived-anchor
                   frame "diff.lines"
                   '((:path . "path") (:line-number . "line-number")
                     (:sign . "sign"))
                   base))
      ('blob      (gitq--pinned base frame '((:sha . "sha") (:path . "path"))))
      ('line      (gitq--pinned base frame '((:commit-sha . "commit-sha")
                                             (:path . "path")
                                             (:line-number . "line-number"))))
      ('worktree  (gitq--pinned base frame '((:path . "path"))))
      (_          (gitq--pinned base frame nil)))))

(defun gitq--refine-moves (base)
  "Continuation moves for the pipeline BASE, as (MOVE KIND DESCRIPTION).
Two questions to the binary and no grammar of our own: the steps and
terminals valid after BASE, and the morphisms whose domain the current
frame shape satisfies.  The morphisms come back flattened into whole
\"via X\" moves — they are the interesting continuations, and each one
has already been type-checked against the shape under point by the time
it is offered (from a hunk, for instance, only `via commit' and `via
history' survive).  `via' itself is dropped: every way to spell it is
already in the list."
  (let ((morphisms (gitq--complete-candidates (concat base " via ")))
        (steps     (gitq--complete-candidates (concat base " "))))
    (append (mapcar (lambda (m) (cons (concat "via " (car m)) (cdr m))) morphisms)
            (seq-remove (lambda (s) (equal (car s) "via")) steps))))

(defun gitq--refine-moves-mode-p (string)
  "Non-nil while STRING is still choosing a whole move.
A move is offered as one unit (`via diff.hunks'), so the first token is
completed against the move list; the moment STRING contains whitespace
the pipeline is growing and per-token completion takes over."
  (not (string-match-p "[ \t]" string)))

(defun gitq--refine-table (base moves)
  "Completion table for refining BASE, offering MOVES then growing.
While the first token is being chosen, completes against MOVES — whole
continuations, already type-checked against the shape under point.
Once the tail contains whitespace the query is growing, and completion
delegates to the same per-token machinery the gitq prompt uses,
anchored at BASE: after `where' that is the fields of the current
shape, after `via' the morphisms, and so on.

Without this the refinement prompt was a single step and stopped dead:
picking `where' left no way to reach the fields, because the move list
was computed once and never asked the binary anything again."
  (lambda (string predicate action)
    (cond
     ((eq action 'metadata)
      '(metadata (category . gitq-token)
                 (display-sort-function . identity)
                 (cycle-sort-function . identity)
                 (affixation-function . gitq--affixate)))
     ((eq (car-safe action) 'boundaries)
      ;; Moves replace the whole tail; growing-pipeline candidates replace
      ;; only the token being typed.
      (cons 'boundaries
            (cons (if (gitq--refine-moves-mode-p string)
                      0
                    (- (length string) (length (gitq--current-token string))))
                  0)))
     ((gitq--refine-moves-mode-p string)
      (complete-with-action action (mapcar #'car moves) string predicate))
     (t
      (let ((cands (gitq--candidates-for (concat base " " string))))
        (setq gitq--annotations cands)
        (complete-with-action action (mapcar #'car cands)
                              (gitq--current-token string) predicate))))))

(defun gitq--refine-preview-input (base)
  "The pipeline the refinement minibuffer would produce, for previewing.
Composes BASE with what has been typed *and* the highlighted candidate,
since the candidate replaces only the token being completed — previewing
the candidate alone would drop the rest of the tail."
  (let* ((contents (minibuffer-contents-no-properties))
         (cand (and (bound-and-true-p vertico--input)
                    (fboundp 'vertico--candidate)
                    (vertico--candidate))))
    (concat base " "
            (if (and cand (not (gitq--refine-moves-mode-p contents)))
                (concat (substring contents 0
                                   (- (length contents)
                                      (length (gitq--current-token contents))))
                        cand)
              (or cand contents)))))

(defun gitq--refine-prompt (base)
  "Prompt for refining BASE, elided on the left when it is long."
  (format "%s ▸ "
          (if (> (length base) 60)
              (concat "…" (substring base (- (length base) 59)))
            base)))

(defun gitq--record (pipeline)
  "Note PIPELINE in the history and the repo it was run from.
A refinement never passes through the minibuffer, so it has to record
itself the way `completing-read' would have."
  (puthash pipeline (gitq--location) gitq--history-locations)
  (unless (equal (car gitq--history) pipeline)
    (push pipeline gitq--history)))

(defun gitq--refine-run (pipeline)
  "Run PIPELINE if the binary can, else hand it to the gitq prompt.
Whether a refinement is finished is the binary's call rather than a
second grammar here: a move that already forms a runnable pipeline
\(`via diff.hunks', `first', `/show') executes straight into the
results buffer, so repeated refinement walks the object graph one level
per keystroke, while a move that still wants an argument (`where',
`take', `grep') simply does not parse yet and lands in the minibuffer
prefilled, where per-token completion and the live preview take over.
Effectful terminals always go to the minibuffer: they are confirmed
with RET, never fired by a single TAB."
  (if (gitq--effectful-terminal-p pipeline)
      (gitq (gitq--read-pipeline "gitq> " (concat pipeline " ")))
    (pcase (gitq--try-frames pipeline)
      (`(:ok . ,frames)
       (gitq--record pipeline)
       (gitq--display frames pipeline))
      (_ (gitq (gitq--read-pipeline "gitq> " (concat pipeline " ")))))))

(defun gitq-results-refine ()
  "Continue the query from the object under point, with completion.
The minibuffer offers the moves that make sense /here/: the morphisms
whose domain the frame under point satisfies, then the steps and
terminals valid after it.  Selecting a complete move runs it, so TAB
after TAB walks down git's object graph — commit, its hunks, the
history of the file a hunk touches — each result buffer being itself a
query you can refine again.

With point on a group header the commit that heads the group is the
anchor; with point on neither (the query line at the top, say) the
whole query is refined instead of a single object."
  (interactive nil gitq-results-mode)
  (gitq--ensure-executable)
  (let* ((frame (or (get-text-property (point) 'gitq-frame)
                    (get-text-property (line-beginning-position) 'gitq-frame)))
         (sha   (or (get-text-property (point) 'gitq-sha)
                    (get-text-property (line-beginning-position) 'gitq-sha)))
         (base  (or (and frame (gitq--frame-anchor frame gitq--buffer-pipeline))
                    sha
                    (gitq--pipeline-without-terminal gitq--buffer-pipeline))))
    (when (or (null base) (string-empty-p base))
      (user-error "gitq: nothing here to refine"))
    (let* ((moves (gitq--refine-moves base))
           (gitq--annotations moves))
      (unless moves
        (user-error "gitq: no further step applies to '%s'" base))
      (gitq--refine-run
       (concat base " "
               (minibuffer-with-setup-hook
                   ;; Preview the move the highlighted candidate would make,
                   ;; not the literal minibuffer contents — the candidate is
                   ;; only the tail, so it is completed against BASE to form
                   ;; the pipeline actually previewed.  Same helper the gitq
                   ;; prompt and the history search use.
                   ;;
                   ;; Effectful terminals are safe to highlight: the preview
                   ;; runs the CLI with --preview, which identifies a
                   ;; terminal but never applies it, so scrolling past
                   ;; /remove shows the commits it *would* touch.
                   (lambda ()
                     (gitq--attach-preview
                      (lambda () (gitq--refine-preview-input base))))
                 ;; Not require-match: the tail grows past the move list
                 ;; (`where author alice' is no single move), and
                 ;; `gitq--refine-run' already decides between running the
                 ;; result and handing it to the gitq prompt.
                 (completing-read (gitq--refine-prompt base)
                                  (gitq--refine-table base moves) nil nil)))))))

;;; Entry point

(defun gitq--effectful-terminal-p (pipeline)
  "Return non-nil if PIPELINE ends in a terminal other than /show.
Such pipelines run through the CLI so the effect happens there; /show
and terminal-less pipelines render into the results buffer instead."
  (when-let ((last-word (car (last (split-string pipeline)))))
    (and (gitq--terminal-token-p last-word)
         (not (equal last-word "/show")))))

;;;###autoload
(defun gitq (pipeline)
  "Execute a GitQ PIPELINE: a whitespace-separated query over git's object graph.

PIPELINE syntax:  source [step...] [/terminal]

Sources:   commits [in RANGE]  HEAD  BRANCH  branches  tags  refs  worktrees  blobs
Steps:     via MORPHISM-PATH  where COND[,COND...]  grep PATTERN  pickaxe PATTERN
           path GLOB  pick FIELD[,...]  [SEL]  sort [-]FIELD
Terminals: /show  /copy  /insert  /count  /branch-off [NAME]  /amend [no-edit|MSG]
           /squash [MSG]  /reword [MSG]  /remove  /delete  /commit [MSG]
           /stage  /mark [LABEL]  /worktree [PATH]

See the gitq CLI's own documentation (doc/gitq.org) for the language;
run \\`gitq --help' for CLI usage.  Invoked from a `gitq-results-mode'
buffer, the minibuffer is pre-filled with the pipeline that produced it."
  (interactive
   (progn
     (gitq--ensure-executable)
     (list (gitq--read-pipeline "gitq> "
                                (when (derived-mode-p 'gitq-results-mode)
                                  gitq--buffer-pipeline)))))
  (gitq--ensure-executable)
  (gitq--record pipeline)
  (if (gitq--effectful-terminal-p pipeline)
      (pcase-let ((`(,code . ,out) (gitq--run pipeline)))
        (if (zerop code)
            (progn
              (when (fboundp 'magit-refresh) (magit-refresh))
              (message "%s" (string-trim out)))
          (user-error "%s" (string-trim out))))
    (gitq--display (gitq--frames pipeline) pipeline)))

(provide 'gitq)
;;; gitq.el ends here
