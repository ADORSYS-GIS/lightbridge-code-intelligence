# Retrieval-Augmented Generation (RAG)

This report details how Lightbridge Code Intelligence utilizes RAG (Retrieval-Augmented Generation) to give the AI agent deep context about the codebase.

## 1. Is RAG used?
**Yes.** RAG is a fundamental component of the Deep Tier review process. Without RAG, the LLM would only be able to see the pull request diff, severely limiting its ability to spot integration bugs, inconsistent patterns, or architectural violations that involve untouched files.

## 2. The Semantic Index Pipeline

Lightbridge builds the retrieval index during the indexing phase (when a repository is first onboarded or when the default branch is updated).

1. **Chunking:** The in-house `lci-codegraph` crate uses Tree-sitter to parse the code. Instead of blindly splitting files by line numbers, it semantically splits the code along logical boundaries (e.g., function definitions, class implementations, structs).
2. **Embedding:** These chunks are sent to an internal OpenAI-compatible embedding gateway (configured to use the `qwen3-embedding-8b` model). The model converts the code chunk into a dense 4096-dimensional mathematical vector.
3. **Storage:** The chunks and their embeddings are stored in PostgreSQL using the `pgvector` extension. Because 4096 dimensions exceed standard HNSW indexing limits, Lightbridge uses exact cosine similarity scans scoped to the specific repository and commit hash.

## 3. How the Agent Uses RAG

During a deep-tier review (e.g., triggered by an `@mention`), the agent operates in a conversational loop. 

When the agent realizes it needs more context about a specific concept—for instance, it sees a call to `verify_hmac_signature` in the diff but doesn't know how that function works—it performs a RAG query:

1. **Tool Call:** The agent calls the `vector_semantic_search` MCP tool with a descriptive query (e.g., "HMAC signature verification logic").
2. **Query Embedding:** The control plane embeds this text query into a 4096-dimensional vector using the same `qwen3-embedding-8b` model.
3. **Vector Search:** The control plane queries PostgreSQL for chunks in the active index snapshot whose vectors have the highest cosine similarity to the query vector.
4. **Augmentation:** The top results (raw code chunks) are returned to the agent and injected into its context window as a system message.
5. **Generation:** The agent proceeds to analyze the pull request, now armed with the exact code definitions it needed.

## 4. Why RAG vs. Full-Repo Context?

Frontier LLMs have massive context windows (e.g., 200k+ tokens), making it theoretically possible to stuff an entire small repository into the prompt. However, Lightbridge relies on dynamic RAG instead for several reasons:

- **Cost:** Stuffing thousands of irrelevant files into a prompt per turn is prohibitively expensive.
- **Attention Degradation:** LLMs suffer from "lost in the middle" syndrome. Feeding them only highly relevant chunks via RAG ensures higher accuracy and reasoning quality.
- **Scale:** Large enterprise repositories easily exceed even the largest context windows, making RAG an absolute necessity.
